use ngc_diagnostics::{NgcError, NgcResult};

use crate::codegen::IvyOutput;
use crate::extract::{DecoratorCommon, ExtractedComponent};

/// Rewrite a TypeScript source string to replace the `@Component` decorator
/// with Ivy static metadata.
///
/// Removes the decorator, updates the `@angular/core` import to include Ivy
/// runtime symbols, and inserts `static ɵfac` and `static ɵcmp` inside the
/// class body. Processes the source in a single pass using span boundaries.
pub fn rewrite_source(
    source: &str,
    component: &ExtractedComponent,
    ivy_output: &IvyOutput,
) -> NgcResult<String> {
    let common = DecoratorCommon {
        decorator_span: component.decorator_span,
        class_body_start: component.class_body_start,
        angular_core_import_span: component.angular_core_import_span,
        other_angular_core_imports: component.other_angular_core_imports.clone(),
    };
    rewrite_source_generic(source, &common, ivy_output)
}

/// Rewrite a TypeScript source string to replace any Angular decorator with
/// Ivy static metadata.
///
/// Generic version that accepts `DecoratorCommon` fields, usable for all
/// Angular decorator types (`@Component`, `@Injectable`, `@Directive`, etc.).
pub fn rewrite_source_generic(
    source: &str,
    common: &DecoratorCommon,
    ivy_output: &IvyOutput,
) -> NgcResult<String> {
    let decorator_start = common.decorator_span.0 as usize;
    let decorator_end = common.decorator_span.1 as usize;
    let class_body_start = common.class_body_start as usize;

    // Validate spans
    if decorator_end > source.len() || class_body_start >= source.len() {
        return Err(NgcError::TemplateCompileError {
            path: std::path::PathBuf::new(),
            message: format!(
                "span out of bounds: decorator_end={decorator_end}, class_body_start={class_body_start}, source_len={}",
                source.len()
            ),
        });
    }
    if decorator_start >= decorator_end || decorator_end > class_body_start {
        return Err(NgcError::TemplateCompileError {
            path: std::path::PathBuf::new(),
            message: format!(
                "invalid span order: decorator=({decorator_start},{decorator_end}), class_body_start={class_body_start}"
            ),
        });
    }

    // Build the new @angular/core import line
    let mut ivy_symbols: Vec<&str> = ivy_output.ivy_imports.iter().map(|s| s.as_str()).collect();
    for imp in &common.other_angular_core_imports {
        if !ivy_symbols.contains(&imp.as_str()) {
            ivy_symbols.push(imp);
        }
    }
    ivy_symbols.sort();
    let new_import = format!(
        "import {{ {} }} from '@angular/core';",
        ivy_symbols.join(", ")
    );

    let mut result = String::new();

    // Segment A+B: everything before the decorator, with import rewriting
    if let Some((import_start, import_end)) = common.angular_core_import_span {
        let import_start = import_start as usize;
        let import_end = import_end as usize;
        result.push_str(&source[..import_start]);
        result.push_str(&new_import);
        result.push_str(&source[import_end..decorator_start]);
    } else {
        result.push_str(&new_import);
        result.push('\n');
        result.push_str(&source[..decorator_start]);
    }

    // Insert child template functions after imports, before the class
    for child_fn in &ivy_output.child_template_functions {
        result.push_str(child_fn);
        result.push('\n');
    }

    // Segment C: skip the decorator and trailing newlines
    let mut pos = decorator_end;
    while pos < source.len() && matches!(source.as_bytes()[pos], b'\n' | b'\r') {
        pos += 1;
    }

    // Segment D: class declaration through opening `{`, with static fields inserted
    result.push_str(&source[pos..=class_body_start]);
    result.push('\n');
    result.push_str("  ");
    result.push_str(&ivy_output.factory_code);
    result.push_str(";\n");
    for field in &ivy_output.static_fields {
        result.push_str("  ");
        result.push_str(field);
        result.push_str(";\n");
    }

    // Segment E: rest of the file (after class body `{`)
    result.push_str(&source[class_body_start + 1..]);

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::IvyOutput;
    use crate::extract::ExtractedComponent;
    use std::collections::BTreeSet;

    // Test source: spans are computed from this exact string
    const TEST_SOURCE: &str = "import { Component } from '@angular/core';\n\n@Component({\n  selector: 'app-root',\n  template: '<h1>Hello</h1>'\n})\nexport class AppComponent {\n  title = 'app';\n}\n";

    fn make_component() -> ExtractedComponent {
        // Compute spans from TEST_SOURCE:
        // import ends at 43 (after ";")
        // @Component starts at 45, ends at 125 (")")
        // "export class AppComponent {" — the { is at 154
        let decorator_start = TEST_SOURCE.find("@Component").unwrap() as u32;
        let decorator_end = TEST_SOURCE[decorator_start as usize..]
            .find(")\n")
            .map(|i| decorator_start + i as u32 + 1)
            .unwrap();
        let class_body_start = TEST_SOURCE.find("AppComponent {").unwrap() as u32 + 14;

        ExtractedComponent {
            class_name: "AppComponent".to_string(),
            selector: "app-root".to_string(),
            template: Some("<h1>Hello</h1>".to_string()),
            template_url: None,
            standalone: true,
            imports_source: None,
            imports_identifiers: Vec::new(),
            decorator_span: (decorator_start, decorator_end),
            class_body_start,
            export_keyword_start: None,
            class_keyword_start: 0,
            angular_core_import_span: Some((0, 43)),
            other_angular_core_imports: Vec::new(),
            styles_source: None,
            inline_styles: Vec::new(),
            style_urls: Vec::new(),
            input_properties: Vec::new(),
            host_listeners: Vec::new(),
            host_bindings: Vec::new(),
            animations_source: None,
            host_directives_source: None,
            signal_inputs: Vec::new(),
            signal_outputs: Vec::new(),
            signal_models: Vec::new(),
            signal_queries: Vec::new(),
            change_detection: None,
        }
    }

    fn make_ivy_output() -> IvyOutput {
        IvyOutput {
            factory_code: "static \u{0275}fac = function AppComponent_Factory(t: any) { return new (t || AppComponent)(); }".to_string(),
            static_fields: vec!["static \u{0275}cmp = \u{0275}\u{0275}defineComponent({\n    type: AppComponent\n  })".to_string()],
            child_template_functions: Vec::new(),
            ivy_imports: BTreeSet::from([
                "\u{0275}\u{0275}defineComponent".to_string(),
                "\u{0275}\u{0275}element".to_string(),
            ]),
            consts: Vec::new(),
        }
    }

    #[test]
    fn test_rewrite_removes_decorator() {
        let component = make_component();
        let ivy = make_ivy_output();
        let result = rewrite_source(TEST_SOURCE, &component, &ivy).expect("should rewrite");
        assert!(!result.contains("@Component"));
        assert!(result.contains("\u{0275}\u{0275}defineComponent"));
        assert!(result.contains("class AppComponent"));
        assert!(result.contains("title = 'app'"));
    }

    #[test]
    fn test_rewrite_updates_imports() {
        let component = make_component();
        let ivy = make_ivy_output();
        let result = rewrite_source(TEST_SOURCE, &component, &ivy).expect("should rewrite");
        assert!(result.contains("\u{0275}\u{0275}defineComponent"));
        assert!(result.contains("\u{0275}\u{0275}element"));
        assert!(!result.contains("import { Component }"));
    }

    #[test]
    fn test_rewrite_inserts_static_fields() {
        let component = make_component();
        let ivy = make_ivy_output();
        let result = rewrite_source(TEST_SOURCE, &component, &ivy).expect("should rewrite");
        assert!(result.contains("static \u{0275}fac"));
        assert!(result.contains("static \u{0275}cmp"));
        // Static fields should be inside the class body
        let class_start = result.find("class AppComponent").unwrap();
        let fac_pos = result.find("static \u{0275}fac").unwrap();
        assert!(fac_pos > class_start, "ɵfac should be inside the class");
    }
}
