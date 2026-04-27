//! AOT codegen for Angular's signal-based authoring APIs.
//!
//! Folds [`SignalInputSpec`] / [`SignalOutputSpec`] / [`SignalModelSpec`]
//! / [`SignalQuerySpec`] (extracted from class field initialisers) into
//! the runtime Ivy property fragments that go into `ɵɵdefineComponent` /
//! `ɵɵdefineDirective`:
//!
//! * `inputs: { foo: [1, 'public', 'foo'], ... }` — `SignalBased` flag
//!   (bit 0) set, `HasDecoratorInputTransform` (bit 1) added when the
//!   field declares `transform`.
//! * `outputs: { changed: 'changed', ... }`.
//! * `viewQuery` / `contentQueries` functions that dispatch to
//!   `ɵɵviewQuerySignal` / `ɵɵcontentQuerySignal` and emit a single
//!   `ɵɵqueryAdvance()` per query in the update block.
//!
//! Decorator-style `@Input` / `@Output` entries that the AOT extractor
//! has already produced (as bare property names or raw source strings)
//! are merged in as identity entries so a class can mix decorator and
//! signal authoring without losing either side.

use std::collections::BTreeSet;

use crate::extract::{SignalInputSpec, SignalModelSpec, SignalOutputSpec, SignalQuerySpec};

/// Result of compiling all signal-API class fields into the runtime
/// fragments needed by `ɵɵdefineComponent` / `ɵɵdefineDirective`.
///
/// Each `*_prop` is either an empty string (caller skips) or a
/// `<key>: <value>` fragment ready to splice into the comma-separated
/// definition body.
pub struct CompiledSignalMembers {
    /// Body of the `inputs` map (without `inputs: { ... }` wrapping) —
    /// only the inner entries. Empty when no signal inputs/models exist
    /// AND no decorator-style inputs were merged in.
    pub inputs_entries: Vec<String>,
    /// Body of the `outputs` map (entries only).
    pub outputs_entries: Vec<String>,
    /// Full `viewQuery: function(rf, ctx) { ... }` fragment, or `None`.
    pub view_query_prop: Option<String>,
    /// Full `contentQueries: function(rf, ctx, directiveIndex) { ... }`
    /// fragment, or `None`.
    pub content_queries_prop: Option<String>,
    /// Ivy runtime symbols referenced by the emitted code. The caller
    /// inserts these into its import set so the rewrite step pulls them
    /// in from `@angular/core`.
    pub ivy_imports: BTreeSet<String>,
}

/// Compile every signal-API spec on a class into the matching Ivy
/// runtime fragments.
///
/// `decorator_inputs` is the bare-property-name list produced by the
/// pre-existing `@Input()` extraction (`ExtractedComponent::input_properties`);
/// passing it here keeps signal- and decorator-style inputs together in
/// the same `inputs` map. The same goes for `outputs_source` — when a
/// `@Directive`'s decorator literal carries an `outputs:` array, the
/// existing entries are surfaced first, and signal `output()` / `model()`
/// fields are appended.
pub fn compile_signal_members(
    decorator_inputs: &[String],
    signal_inputs: &[SignalInputSpec],
    signal_outputs: &[SignalOutputSpec],
    signal_models: &[SignalModelSpec],
    signal_queries: &[SignalQuerySpec],
) -> CompiledSignalMembers {
    let mut ivy_imports = BTreeSet::new();
    let mut input_entries = Vec::new();
    let mut output_entries = Vec::new();

    // Decorator-style @Input() — already compiled to identity entries
    // upstream. Replicate that shape so the maps stay flat strings unless
    // a flag/alias forces the array form.
    for prop in decorator_inputs {
        input_entries.push(format!("{prop}: '{prop}'"));
    }

    for spec in signal_inputs {
        input_entries.push(format_signal_input_entry(
            &spec.property_name,
            spec.alias.as_deref(),
            spec.is_required,
            spec.transform_source.as_deref(),
        ));
    }

    for spec in signal_models {
        // `model<T>()` is sugar for a paired signal input + `<name>Change`
        // output. The output's public name follows the input alias when
        // one is set (`model({ alias: 'pub' })` → input `pub` + output
        // `pubChange`), matching Angular's compiler.
        let public = spec.alias.as_deref().unwrap_or(&spec.property_name);
        input_entries.push(format_signal_input_entry(
            &spec.property_name,
            spec.alias.as_deref(),
            spec.is_required,
            None,
        ));
        let change_name = format!("{public}Change");
        output_entries.push(format!("{}: '{}'", change_name, change_name));
    }

    for spec in signal_outputs {
        let public = spec.alias.as_deref().unwrap_or(&spec.property_name);
        output_entries.push(format!("{}: '{}'", spec.property_name, public));
    }

    // Split queries by view vs. content so we can build the two
    // dispatch functions independently. Order is preserved so the
    // runtime's query-index advance lines up with the declaration order
    // on the class.
    let (view_qs, content_qs): (Vec<_>, Vec<_>) =
        signal_queries.iter().partition(|q| q.kind.is_view());

    let view_query_prop = if view_qs.is_empty() {
        None
    } else {
        ivy_imports.insert("\u{0275}\u{0275}viewQuerySignal".to_string());
        ivy_imports.insert("\u{0275}\u{0275}queryAdvance".to_string());
        Some(format!(
            "viewQuery: {}",
            build_query_function(&view_qs, false)
        ))
    };

    let content_queries_prop = if content_qs.is_empty() {
        None
    } else {
        ivy_imports.insert("\u{0275}\u{0275}contentQuerySignal".to_string());
        ivy_imports.insert("\u{0275}\u{0275}queryAdvance".to_string());
        Some(format!(
            "contentQueries: {}",
            build_query_function(&content_qs, true)
        ))
    };

    CompiledSignalMembers {
        inputs_entries: input_entries,
        outputs_entries: output_entries,
        view_query_prop,
        content_queries_prop,
        ivy_imports,
    }
}

/// Format a single `inputs` map entry from a [`SignalInputSpec`] /
/// [`SignalModelSpec`]. The signal-based bit is always set; the
/// transform bit is added only when a transform expression is present.
fn format_signal_input_entry(
    property_name: &str,
    alias: Option<&str>,
    _is_required: bool,
    transform_source: Option<&str>,
) -> String {
    let public = alias.unwrap_or(property_name);
    let mut flags: u32 = 1; // SignalBased
    if transform_source.is_some() {
        flags |= 2; // HasDecoratorInputTransform
    }
    if let Some(t) = transform_source {
        format!("{property_name}: [{flags}, '{public}', '{property_name}', {t}]")
    } else {
        format!("{property_name}: [{flags}, '{public}', '{property_name}']")
    }
}

/// Build `function(rf, ctx[, directiveIndex]) { if (rf & 1) {...} if (rf & 2) {...} }`
/// for the given queries. `is_content` toggles the directiveIndex parameter
/// and dispatches to `ɵɵcontentQuerySignal` instead of `ɵɵviewQuerySignal`.
fn build_query_function(queries: &[&SignalQuerySpec], is_content: bool) -> String {
    let create_fn = if is_content {
        "\u{0275}\u{0275}contentQuerySignal"
    } else {
        "\u{0275}\u{0275}viewQuerySignal"
    };
    let advance_fn = "\u{0275}\u{0275}queryAdvance";

    let mut create_stmts = Vec::with_capacity(queries.len());
    let mut update_stmts = Vec::with_capacity(queries.len());

    for q in queries {
        let flags = compute_signal_query_flags(q);
        let read_arg = if let Some(ref read) = q.read_source {
            format!(", {read}")
        } else {
            String::new()
        };
        let target = format!("ctx.{}", q.property_name);

        if is_content {
            create_stmts.push(format!(
                "{create_fn}(directiveIndex, {target}, {}, {flags}{read_arg});",
                q.predicate_source
            ));
        } else {
            create_stmts.push(format!(
                "{create_fn}({target}, {}, {flags}{read_arg});",
                q.predicate_source
            ));
        }
        update_stmts.push(format!("{advance_fn}();"));
    }

    let mut body = String::from("if (rf & 1) { ");
    for s in &create_stmts {
        body.push_str(s);
        body.push(' ');
    }
    body.push_str("} if (rf & 2) { ");
    for s in &update_stmts {
        body.push_str(s);
        body.push(' ');
    }
    body.push('}');

    if is_content {
        format!("function(rf, ctx, directiveIndex) {{ {body} }}")
    } else {
        format!("function(rf, ctx) {{ {body} }}")
    }
}

/// Compute the runtime `QueryFlags` integer for a signal-based query.
/// Mirrors the linker: bit 0 = descendants, bit 1 = isStatic,
/// bit 2 = emitDistinctChangesOnly (set for plural `*Children` variants
/// to match Angular's `QueryList` distinct-emission semantics).
fn compute_signal_query_flags(q: &SignalQuerySpec) -> u32 {
    let mut flags: u32 = 0;
    if q.kind.default_descendants() {
        flags |= 1;
    }
    if q.is_static {
        flags |= 2;
    }
    if !q.kind.is_first() {
        flags |= 4;
    }
    flags
}

/// Wrap a list of `inputs` / `outputs` entries in the `<key>: { ... }`
/// fragment shape used in the define call body. Returns `None` when the
/// list is empty so callers don't emit an empty map.
pub fn format_map(key: &str, entries: &[String]) -> Option<String> {
    if entries.is_empty() {
        None
    } else {
        Some(format!("{key}: {{ {} }}", entries.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::SignalQueryKind;

    fn input(name: &str) -> SignalInputSpec {
        SignalInputSpec {
            property_name: name.into(),
            alias: None,
            is_required: false,
            transform_source: None,
        }
    }

    /// Plain `input()` should set the `SignalBased` flag (bit 0) so
    /// the runtime knows the field is a `WritableSignal` and not a
    /// plain assignable property.
    #[test]
    fn signal_input_emits_signal_flag() {
        let r = compile_signal_members(&[], &[input("name")], &[], &[], &[]);
        assert!(
            r.inputs_entries[0].contains("name: [1, 'name', 'name']"),
            "got: {:?}",
            r.inputs_entries
        );
    }

    /// `input(0, { transform: trim })` adds the
    /// `HasDecoratorInputTransform` flag (bit 1) AND surfaces the
    /// transform reference verbatim as the array's 4th element so the
    /// runtime can call it on each value.
    #[test]
    fn signal_input_with_transform_emits_transform_flag() {
        let mut spec = input("name");
        spec.transform_source = Some("trimString".into());
        let r = compile_signal_members(&[], &[spec], &[], &[], &[]);
        assert!(r.inputs_entries[0].contains("name: [3, 'name', 'name', trimString]"));
    }

    /// Aliased `input(0, { alias: 'pub' })` keeps `publicName` in
    /// position 1 and the class-property name in position 2 — flipping
    /// these two would silently break template binding name resolution.
    #[test]
    fn signal_input_with_alias_swaps_public_name() {
        let mut spec = input("internal");
        spec.alias = Some("public".into());
        let r = compile_signal_members(&[], &[spec], &[], &[], &[]);
        assert!(r.inputs_entries[0].contains("internal: [1, 'public', 'internal']"));
    }

    /// `output()` is purely a name in the runtime `outputs` map —
    /// no flag bits. With no alias the public name equals the field
    /// name (identity).
    #[test]
    fn signal_output_emits_identity_pair() {
        let spec = SignalOutputSpec {
            property_name: "changed".into(),
            alias: None,
        };
        let r = compile_signal_members(&[], &[], &[spec], &[], &[]);
        assert!(r.outputs_entries[0].contains("changed: 'changed'"));
    }

    /// `output({ alias: 'pub' })` produces `propName: 'publicName'`
    /// so the runtime's `outputs` map carries the field name as key
    /// (used to look up the EventEmitter on the instance) and the
    /// public name as value (used to match `(public)="..."` bindings).
    #[test]
    fn signal_output_with_alias() {
        let spec = SignalOutputSpec {
            property_name: "changed".into(),
            alias: Some("publicChanged".into()),
        };
        let r = compile_signal_members(&[], &[], &[spec], &[], &[]);
        assert!(r.outputs_entries[0].contains("changed: 'publicChanged'"));
    }

    /// `model<T>()` desugars to a signal input named after the field
    /// PLUS an output named `<name>Change`. Both entries must appear.
    #[test]
    fn signal_model_emits_input_and_change_output() {
        let spec = SignalModelSpec {
            property_name: "value".into(),
            alias: None,
            is_required: false,
        };
        let r = compile_signal_members(&[], &[], &[], &[spec], &[]);
        assert!(r.inputs_entries[0].contains("value: [1, 'value', 'value']"));
        assert!(r.outputs_entries[0].contains("valueChange: 'valueChange'"));
    }

    /// Aliased `model({ alias: 'pub' })` emits the aliased input AND
    /// `pubChange` (the change-event public name follows the input
    /// alias, NOT the property name).
    #[test]
    fn signal_model_alias_propagates_to_change_event() {
        let spec = SignalModelSpec {
            property_name: "internal".into(),
            alias: Some("public".into()),
            is_required: false,
        };
        let r = compile_signal_members(&[], &[], &[], &[spec], &[]);
        assert!(r.inputs_entries[0].contains("internal: [1, 'public', 'internal']"));
        assert!(r.outputs_entries[0].contains("publicChange: 'publicChange'"));
    }

    /// Decorator-style `@Input()` entries (bare property names) flow
    /// through unchanged so a class mixing both authoring styles
    /// produces a single combined `inputs` map.
    #[test]
    fn decorator_inputs_merge_with_signal_inputs() {
        let r = compile_signal_members(&["plain".into()], &[input("signalInput")], &[], &[], &[]);
        assert!(r.inputs_entries.iter().any(|e| e == "plain: 'plain'"));
        assert!(r
            .inputs_entries
            .iter()
            .any(|e| e.contains("signalInput: [1, 'signalInput', 'signalInput']")));
    }

    /// `viewChild('ref')` lands as a single signal-based view query.
    /// `descendants` defaults to `true` for view queries (flag bit 0)
    /// AND `first: true` (so bit 2 stays clear). Predicate text is
    /// preserved verbatim because it can be either a string literal
    /// (`['ref']`) or a class reference.
    #[test]
    fn signal_view_child_emits_view_query_signal() {
        let q = SignalQuerySpec {
            property_name: "child".into(),
            kind: SignalQueryKind::ViewChild,
            predicate_source: "ChildCmp".into(),
            read_source: None,
            is_static: false,
        };
        let r = compile_signal_members(&[], &[], &[], &[], &[q]);
        let vq = r.view_query_prop.as_ref().expect("expected viewQuery");
        assert!(
            vq.contains("\u{0275}\u{0275}viewQuerySignal(ctx.child, ChildCmp, 1)"),
            "got: {vq}"
        );
        assert!(
            vq.contains("\u{0275}\u{0275}queryAdvance();"),
            "expected queryAdvance() in update block, got: {vq}"
        );
    }

    /// `viewChildren()` (plural) sets both `descendants` (bit 0) and
    /// `emitDistinctChangesOnly` (bit 2) for a flags integer of 5,
    /// matching Angular's compiler emission for `QueryList`-style
    /// multi-result queries.
    #[test]
    fn signal_view_children_sets_distinct_changes_flag() {
        let q = SignalQuerySpec {
            property_name: "children".into(),
            kind: SignalQueryKind::ViewChildren,
            predicate_source: "ChildCmp".into(),
            read_source: None,
            is_static: false,
        };
        let r = compile_signal_members(&[], &[], &[], &[], &[q]);
        let vq = r.view_query_prop.as_ref().expect("viewQuery");
        assert!(
            vq.contains("ɵɵviewQuerySignal(ctx.children, ChildCmp, 5)")
                || vq.contains("\u{0275}\u{0275}viewQuerySignal(ctx.children, ChildCmp, 5)")
        );
    }

    /// `contentChild()` defaults to `descendants: false` (bit 0 clear)
    /// — only the direct children get matched unless the user opts
    /// into descendants explicitly. `first: true` so distinct-changes
    /// is also clear, leaving flags at 0.
    #[test]
    fn signal_content_child_defaults_no_descendants() {
        let q = SignalQuerySpec {
            property_name: "projected".into(),
            kind: SignalQueryKind::ContentChild,
            predicate_source: "SomeDir".into(),
            read_source: None,
            is_static: false,
        };
        let r = compile_signal_members(&[], &[], &[], &[], &[q]);
        let cq = r.content_queries_prop.as_ref().expect("contentQueries");
        assert!(
            cq.contains(
                "\u{0275}\u{0275}contentQuerySignal(directiveIndex, ctx.projected, SomeDir, 0)"
            ),
            "expected directiveIndex-leading content query call with flags=0, got: {cq}"
        );
    }

    /// `viewChild('ref', { read: ElementRef })` propagates the read
    /// token as a trailing arg so the runtime resolves the ElementRef
    /// rather than the matched directive instance.
    #[test]
    fn signal_view_child_with_read_token() {
        let q = SignalQuerySpec {
            property_name: "el".into(),
            kind: SignalQueryKind::ViewChild,
            predicate_source: "['ref']".into(),
            read_source: Some("ElementRef".into()),
            is_static: false,
        };
        let r = compile_signal_members(&[], &[], &[], &[], &[q]);
        let vq = r.view_query_prop.as_ref().expect("viewQuery");
        assert!(vq.contains("\u{0275}\u{0275}viewQuerySignal(ctx.el, ['ref'], 1, ElementRef)"));
    }

    /// Mixing view + content signal queries on the same class produces
    /// both functions, each with its own advance counts. View queries
    /// must not leak into the content function (and vice versa) — they
    /// share an LView slot pool but live in distinct create/refresh
    /// blocks.
    #[test]
    fn mixed_view_and_content_queries_split_into_two_props() {
        let queries = vec![
            SignalQuerySpec {
                property_name: "child".into(),
                kind: SignalQueryKind::ViewChild,
                predicate_source: "C".into(),
                read_source: None,
                is_static: false,
            },
            SignalQuerySpec {
                property_name: "projected".into(),
                kind: SignalQueryKind::ContentChild,
                predicate_source: "P".into(),
                read_source: None,
                is_static: false,
            },
        ];
        let r = compile_signal_members(&[], &[], &[], &[], &queries);
        let vq = r.view_query_prop.expect("viewQuery");
        let cq = r.content_queries_prop.expect("contentQueries");
        assert!(vq.contains("ctx.child"));
        assert!(!vq.contains("ctx.projected"));
        assert!(cq.contains("ctx.projected"));
        assert!(!cq.contains("ctx.child"));
    }
}
