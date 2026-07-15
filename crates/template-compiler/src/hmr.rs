//! Hot Module Replacement (HMR) codegen for `@Component` classes.
//!
//! Mirrors Angular's esbuild HMR exactly (reverse-engineered from
//! `@angular/compiler` + `@angular/core`):
//!
//! * A per-component **initializer IIFE** is appended to the component module.
//!   It registers an `import.meta.hot` listener for `angular:component-update`
//!   and, on a matching id, dynamically imports the component's update module
//!   and calls `i0.ɵɵreplaceMetadata` to swap the definition in place.
//! * A separate **update module** is produced (not written to disk — served on
//!   demand by the dev server at `/@ng/component?c=<id>`). Its `export default`
//!   is a function that re-applies the freshly compiled `ɵcmp` to the existing
//!   class. It carries no imports: the `@angular/core` namespace arrives as the
//!   `ɵɵnamespaces` array argument and local template dependencies arrive as
//!   positional parameters.
//!
//! The update module reassigns only `ɵcmp` (template + styles), never `ɵfac`:
//! the serve command only emits a component update when a component's factory
//! and class body are byte-identical to the previous build (template-/style-
//! only change), so the running factory is already correct. This keeps the
//! update module free of constructor-DI symbols it would otherwise need in
//! scope.

use crate::codegen::IvyOutput;

/// Percent-encode `input` with JavaScript `encodeURIComponent` semantics.
///
/// `encodeURIComponent` leaves the "unreserved" set unescaped —
/// `A-Z a-z 0-9 - _ . ! ~ * ' ( )` — and `%XX`-encodes every other byte
/// (UTF-8, uppercase hex). This is deliberately *not* a generic URL encoder:
/// the result is the component id contract shared by the compiler-emitted
/// initializer, the dev-server registry key, and the running app's fetch URL.
pub fn encode_uri_component(input: &str) -> String {
    fn is_unreserved(b: u8) -> bool {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
    }
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(
                char::from_digit((b >> 4) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
            out.push(
                char::from_digit((b & 0xf) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
        }
    }
    out
}

/// Compute a component's HMR id: `encodeURIComponent("<relpath>@<ClassName>")`.
///
/// `relpath` is `file_path` relative to `project_root` with `/` separators
/// (falling back to the file's own components when it isn't under the root).
/// The compiler is the sole producer of this id — it is embedded verbatim in
/// the initializer and used as the dev-server registry key — so the exact
/// `project_root` only needs to be applied *consistently*, not to match any
/// external value.
pub fn component_hmr_id(
    project_root: &std::path::Path,
    file_path: &std::path::Path,
    class_name: &str,
) -> String {
    let rel = file_path.strip_prefix(project_root).unwrap_or(file_path);
    // Join components with `/` so the id is stable across platforms.
    let rel_str = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/");
    encode_uri_component(&format!("{rel_str}@{class_name}"))
}

/// Build the per-component HMR initializer IIFE appended to the component
/// module. `locals` are the template dependency identifiers (the `imports:`
/// entries), passed to `ɵɵreplaceMetadata` in the same order the update
/// module declares its parameters.
///
/// References `i0` (the `import * as i0 from '@angular/core'` namespace the
/// HMR rewrite adds) for `ɵɵreplaceMetadata`, and the component class plus
/// each local by binding — all resolved in the module's top-level scope.
pub fn build_initializer(class_name: &str, id: &str, locals: &[String]) -> String {
    let locals_arr = locals.join(", ");
    // Plain JS (no TS annotations) so it survives ts-transform untouched.
    format!(
        "(() => {{\n\
         var __ngId = '{id}';\n\
         function {class_name}_HmrLoad(t) {{\n\
         return import('./@ng/component?c=' + __ngId + '&t=' + encodeURIComponent(t)).then(\n\
         m => m.default && i0.\u{0275}\u{0275}replaceMetadata({class_name}, m.default, [i0], [{locals_arr}], import.meta, __ngId));\n\
         }}\n\
         if (import.meta.hot) {{\n\
         import.meta.hot.on('angular:component-update', d => {{ if (d.id === __ngId) {class_name}_HmrLoad(d.timestamp); }});\n\
         }}\n\
         }})();\n"
    )
}

/// Build the TypeScript source of a component's HMR update module.
///
/// The caller runs the result through `ngc_ts_transform::transform_source` to
/// strip TypeScript annotations (and validate it as JS) before serving it.
///
/// Reuses the existing Ivy codegen verbatim via the linker's proven var-alias
/// technique: every runtime symbol (`ɵɵdefineComponent`, `ɵɵelement`, …) is
/// rebound from the `i0` namespace as a local `var`, so the unmodified `ɵcmp`
/// definition string — which references those symbols by their bare names —
/// resolves against the locals. Template dependency identifiers resolve to the
/// function's positional parameters.
pub fn build_update_module_ts(class_name: &str, ivy: &IvyOutput, locals: &[String]) -> String {
    let mut out = String::new();

    // export default function X_UpdateMetadata(X, ɵɵnamespaces, dep1, dep2) {
    out.push_str(&format!(
        "export default function {class_name}_UpdateMetadata({class_name}, \u{0275}\u{0275}namespaces"
    ));
    for local in locals {
        out.push_str(", ");
        out.push_str(local);
    }
    out.push_str(") {\n");

    // Rebind every runtime symbol from the core namespace so the reused def
    // text (which uses bare `ɵɵ…` names) resolves without imports.
    out.push_str("  const i0 = \u{0275}\u{0275}namespaces[0];\n");
    for sym in &ivy.ivy_imports {
        out.push_str(&format!("  var {sym} = i0.{sym};\n"));
    }

    // Child template functions referenced by the main template, in scope.
    for child in &ivy.child_template_functions {
        out.push_str(child);
        out.push('\n');
    }

    // Reassign the component definition. `static_fields[0]` is
    // `static ɵcmp = ɵɵdefineComponent({...})`; turn the class-field form into
    // an assignment statement on the class passed in as the first parameter.
    let def = ivy.static_fields.first().map(|s| s.as_str()).unwrap_or("");
    let def_expr = def.strip_prefix("static \u{0275}cmp = ").unwrap_or(def);
    out.push_str(&format!("  {class_name}.\u{0275}cmp = {def_expr};\n"));

    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    #[test]
    fn encode_uri_component_matches_js_semantics() {
        assert_eq!(
            encode_uri_component("src/app/app.component.ts@AppComponent"),
            "src%2Fapp%2Fapp.component.ts%40AppComponent"
        );
        // Unreserved set is left untouched.
        assert_eq!(encode_uri_component("-_.!~*'()"), "-_.!~*'()");
        // Spaces, slashes, and unicode are percent-encoded (UTF-8, upper hex).
        assert_eq!(encode_uri_component("a b/c"), "a%20b%2Fc");
        assert_eq!(encode_uri_component("é"), "%C3%A9");
    }

    #[test]
    fn component_hmr_id_is_relative_with_forward_slashes() {
        let id = component_hmr_id(
            Path::new("/proj"),
            Path::new("/proj/src/app/app.component.ts"),
            "AppComponent",
        );
        assert_eq!(id, "src%2Fapp%2Fapp.component.ts%40AppComponent");
    }

    #[test]
    fn component_hmr_id_falls_back_when_not_under_root() {
        let id = component_hmr_id(
            Path::new("/other"),
            Path::new("/proj/app.component.ts"),
            "App",
        );
        // Not under root → encodes the full path it was given.
        assert!(id.ends_with("app.component.ts%40App"));
        assert!(id.contains("%2F"));
    }

    #[test]
    fn initializer_embeds_id_locals_and_replace_metadata() {
        let init = build_initializer(
            "AppComponent",
            "the%2Fid%40AppComponent",
            &["RouterOutlet".into(), "MyPipe".into()],
        );
        assert!(init.contains("var __ngId = 'the%2Fid%40AppComponent';"));
        assert!(init.contains("i0.\u{0275}\u{0275}replaceMetadata(AppComponent, m.default, [i0], [RouterOutlet, MyPipe], import.meta, __ngId)"));
        assert!(
            init.contains("import('./@ng/component?c=' + __ngId + '&t=' + encodeURIComponent(t))")
        );
        assert!(init.contains("import.meta.hot.on('angular:component-update'"));
    }

    #[test]
    fn update_module_rebinds_namespace_and_assigns_cmp() {
        let ivy = IvyOutput {
            factory_code: "static \u{0275}fac = function App_Factory(t) { return new (t || App)(); }".into(),
            static_fields: vec![
                "static \u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: App, template: function App_Template(rf, ctx) {} })".into(),
            ],
            child_template_functions: vec!["function App_div_0_Template(rf, ctx) {}".into()],
            ivy_imports: {
                let mut s = BTreeSet::new();
                s.insert("\u{0275}\u{0275}defineComponent".to_string());
                s.insert("\u{0275}\u{0275}element".to_string());
                s
            },
            consts: vec![],
        };
        let module = build_update_module_ts("App", &ivy, &["RouterOutlet".into()]);
        assert!(module.contains("export default function App_UpdateMetadata(App, \u{0275}\u{0275}namespaces, RouterOutlet)"));
        assert!(module.contains("const i0 = \u{0275}\u{0275}namespaces[0];"));
        assert!(module
            .contains("var \u{0275}\u{0275}defineComponent = i0.\u{0275}\u{0275}defineComponent;"));
        assert!(module.contains("function App_div_0_Template"));
        assert!(module.contains("App.\u{0275}cmp = \u{0275}\u{0275}defineComponent({"));
        // The update module must not reassign the factory (template/style-only).
        assert!(!module.contains(".\u{0275}fac"));
    }
}
