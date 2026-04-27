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
        // Signal-based inputs DO NOT propagate the `transform` into the
        // runtime `inputs` def — Angular's compiler keeps it on the
        // signal field's `InputSignalNode.transformFn` (set by the
        // `input(default, { transform })` factory at instantiation
        // time). The runtime's `writeToDirectiveInput` always prefers
        // `inputSignalNode.transformFn` over the def's `transform`, so
        // emitting the transform in the def is at best dead weight and
        // at worst confusing diff noise vs. `ng build`.
        input_entries.push(format_signal_input_entry(
            &spec.property_name,
            spec.alias.as_deref(),
            spec.is_required,
            None,
        ));
    }

    for spec in signal_models {
        // `model<T>()` is sugar for a paired signal input + `<publicName>Change`
        // output. Angular's runtime inverts the outputs map to
        // `{ publicName: classPropName }`, then on the change event it
        // looks up `instance[classPropName]` to subscribe to. For a
        // model the change-event source is the model SIGNAL itself
        // (`instance.<field>`) — NOT a separate field named
        // `<field>Change`. So the outputs entry must be keyed by the
        // model's class-property name and valued with the public event
        // name (`<publicName>Change`); emitting
        // `{ <publicName>Change: '<publicName>Change' }` would make the
        // runtime try to subscribe to a non-existent
        // `instance.<publicName>Change` field and silently drop the
        // two-way binding.
        let public = spec.alias.as_deref().unwrap_or(&spec.property_name);
        input_entries.push(format_signal_input_entry(
            &spec.property_name,
            spec.alias.as_deref(),
            spec.is_required,
            None,
        ));
        output_entries.push(format!("{}: '{}Change'", spec.property_name, public));
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

/// Format a single `inputs` map entry, matching Angular's compact
/// emission rules:
///
/// * `[flags, publicName]` when `publicName == classPropertyName` and
///   no decorator transform.
/// * `[flags, publicName, classPropertyName]` when the names differ.
/// * `[flags, publicName, classPropertyName, transform]` when a
///   decorator-style transform is in play (only ever for non-signal
///   inputs — signal inputs keep their transform on the signal field).
///
/// `SignalBased` (bit 0) is always set here because every caller is a
/// signal-API field. `HasDecoratorInputTransform` (bit 1) is only set
/// when `transform_source` is provided — and signal-input callers pass
/// `None`, so it's effectively for decorator inputs going forward.
fn format_signal_input_entry(
    property_name: &str,
    alias: Option<&str>,
    _is_required: bool,
    transform_source: Option<&str>,
) -> String {
    let public = alias.unwrap_or(property_name);
    let names_differ = public != property_name;
    let mut flags: u32 = 1; // SignalBased
    if transform_source.is_some() {
        flags |= 2; // HasDecoratorInputTransform
    }
    match (names_differ, transform_source) {
        (false, None) => format!("{property_name}: [{flags}, '{public}']"),
        (true, None) => format!("{property_name}: [{flags}, '{public}', '{property_name}']"),
        (_, Some(t)) => {
            format!("{property_name}: [{flags}, '{public}', '{property_name}', {t}]")
        }
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
///
/// Matches Angular's compiler-cli behaviour bit-for-bit:
/// * bit 0 (`descendants`) — kind-specific default unless the user
///   passed an explicit `descendants:` option. Only `contentChildren`
///   defaults to `false`; `viewChild` / `viewChildren` / `contentChild`
///   all default to `true`.
/// * bit 1 (`isStatic`) — from the `static:` option.
/// * bit 2 (`emitDistinctChangesOnly`) — **always set for signal queries**,
///   regardless of `first` / multi. Unlike `QueryList`-backed decorator
///   queries (which only set this bit when emitting many results),
///   Angular's signal-query compiler unconditionally sets it.
fn compute_signal_query_flags(q: &SignalQuerySpec) -> u32 {
    let mut flags: u32 = 4; // emitDistinctChangesOnly — always set for signal queries
    if q.descendants
        .unwrap_or_else(|| q.kind.default_descendants())
    {
        flags |= 1;
    }
    if q.is_static {
        flags |= 2;
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

    /// Plain `input()` should emit the compact 2-element form
    /// `[flags, publicName]` since the public name matches the class
    /// property name. SignalBased flag (bit 0) is always set so the
    /// runtime knows the field is a `WritableSignal` and not a plain
    /// assignable property.
    #[test]
    fn signal_input_emits_signal_flag() {
        let r = compile_signal_members(&[], &[input("name")], &[], &[], &[]);
        assert!(
            r.inputs_entries[0].contains("name: [1, 'name']"),
            "got: {:?}",
            r.inputs_entries
        );
    }

    /// `input(0, { transform: trim })` keeps the transform on the
    /// signal field — Angular's compiler does NOT replicate it into
    /// the inputs def for signal inputs (the runtime reads
    /// `inputSignalNode.transformFn` directly). So flags stay at
    /// SignalBased only and the entry is the compact 2-element form.
    #[test]
    fn signal_input_with_transform_keeps_transform_on_signal() {
        let mut spec = input("name");
        spec.transform_source = Some("trimString".into());
        let r = compile_signal_members(&[], &[spec], &[], &[], &[]);
        assert!(
            r.inputs_entries[0].contains("name: [1, 'name']"),
            "expected compact form for signal input (transform stays on signal field), got: {:?}",
            r.inputs_entries
        );
        assert!(
            !r.inputs_entries[0].contains("trimString"),
            "transform must NOT appear in the def for signal inputs: {:?}",
            r.inputs_entries
        );
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
    /// PLUS an output. The outputs entry is keyed by the class-property
    /// name so the runtime can locate `instance.<field>` (the model
    /// signal it subscribes to) — the public event name `<field>Change`
    /// is the VALUE.
    #[test]
    fn signal_model_emits_input_and_change_output() {
        let spec = SignalModelSpec {
            property_name: "value".into(),
            alias: None,
            is_required: false,
        };
        let r = compile_signal_members(&[], &[], &[], &[spec], &[]);
        assert!(r.inputs_entries[0].contains("value: [1, 'value']"));
        assert!(r.outputs_entries[0].contains("value: 'valueChange'"));
    }

    /// Aliased `model({ alias: 'pub' })` emits the aliased input AND
    /// the change output's PUBLIC name uses the alias (`pubChange`),
    /// while the outputs map's KEY remains the original class property
    /// name — that's what the runtime looks up on the instance.
    #[test]
    fn signal_model_alias_propagates_to_change_event() {
        let spec = SignalModelSpec {
            property_name: "internal".into(),
            alias: Some("public".into()),
            is_required: false,
        };
        let r = compile_signal_members(&[], &[], &[], &[spec], &[]);
        assert!(r.inputs_entries[0].contains("internal: [1, 'public', 'internal']"));
        assert!(r.outputs_entries[0].contains("internal: 'publicChange'"));
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
            .any(|e| e.contains("signalInput: [1, 'signalInput']")));
    }

    fn query(name: &str, kind: SignalQueryKind, predicate: &str) -> SignalQuerySpec {
        SignalQuerySpec {
            property_name: name.into(),
            kind,
            predicate_source: predicate.into(),
            read_source: None,
            is_static: false,
            descendants: None,
        }
    }

    /// `viewChild('ref')` lands as a single signal-based view query.
    /// Angular's compiler ALWAYS sets the `emitDistinctChangesOnly` bit
    /// (bit 2 = 4) for signal queries, regardless of whether the query
    /// is single (`first: true`) or multi. With the kind's default
    /// `descendants: true` (bit 0 = 1), flags = 5.
    #[test]
    fn signal_view_child_emits_view_query_signal() {
        let q = query("child", SignalQueryKind::ViewChild, "ChildCmp");
        let r = compile_signal_members(&[], &[], &[], &[], &[q]);
        let vq = r.view_query_prop.as_ref().expect("expected viewQuery");
        assert!(
            vq.contains("\u{0275}\u{0275}viewQuerySignal(ctx.child, ChildCmp, 5)"),
            "got: {vq}"
        );
        assert!(
            vq.contains("\u{0275}\u{0275}queryAdvance();"),
            "expected queryAdvance() in update block, got: {vq}"
        );
    }

    /// `viewChildren()` shares the same flags shape as `viewChild`
    /// because the `emitDistinctChangesOnly` bit is unconditionally set
    /// for signal queries (the runtime distinguishes single vs. multi
    /// by other means — what the bit really gates is QueryList semantics
    /// that signal queries no longer use).
    #[test]
    fn signal_view_children_sets_distinct_changes_flag() {
        let q = query("children", SignalQueryKind::ViewChildren, "ChildCmp");
        let r = compile_signal_members(&[], &[], &[], &[], &[q]);
        let vq = r.view_query_prop.as_ref().expect("viewQuery");
        assert!(
            vq.contains("\u{0275}\u{0275}viewQuerySignal(ctx.children, ChildCmp, 5)"),
            "got: {vq}"
        );
    }

    /// Signal-based `contentChild()` defaults to `descendants: true`
    /// — opposite of decorator-style `@ContentChild` which defaults to
    /// `descendants: false`. Combined with the always-on
    /// `emitDistinctChangesOnly` bit, flags = 5.
    #[test]
    fn signal_content_child_defaults_descendants_true() {
        let q = query("projected", SignalQueryKind::ContentChild, "SomeDir");
        let r = compile_signal_members(&[], &[], &[], &[], &[q]);
        let cq = r.content_queries_prop.as_ref().expect("contentQueries");
        assert!(
            cq.contains(
                "\u{0275}\u{0275}contentQuerySignal(directiveIndex, ctx.projected, SomeDir, 5)"
            ),
            "expected contentChild defaults to descendants=true (flags=5), got: {cq}"
        );
    }

    /// `contentChildren()` is the only signal-query factory that
    /// defaults to `descendants: false`. Bit 0 stays clear; bit 2 is
    /// always set → flags = 4.
    #[test]
    fn signal_content_children_defaults_no_descendants() {
        let q = query("all", SignalQueryKind::ContentChildren, "SomeDir");
        let r = compile_signal_members(&[], &[], &[], &[], &[q]);
        let cq = r.content_queries_prop.as_ref().expect("contentQueries");
        assert!(
            cq.contains("\u{0275}\u{0275}contentQuerySignal(directiveIndex, ctx.all, SomeDir, 4)"),
            "expected contentChildren defaults to descendants=false (flags=4), got: {cq}"
        );
    }

    /// User-supplied `descendants: false` on a `viewChild` (which
    /// defaults to `true`) must override the kind's default.
    #[test]
    fn signal_query_user_descendants_overrides_default() {
        let mut q = query("child", SignalQueryKind::ViewChild, "C");
        q.descendants = Some(false);
        let r = compile_signal_members(&[], &[], &[], &[], &[q]);
        let vq = r.view_query_prop.as_ref().expect("viewQuery");
        assert!(
            vq.contains("\u{0275}\u{0275}viewQuerySignal(ctx.child, C, 4)"),
            "expected user-supplied descendants:false to clear bit 0, got: {vq}"
        );
    }

    /// `viewChild('ref', { read: ElementRef })` propagates the read
    /// token as a trailing arg so the runtime resolves the ElementRef
    /// rather than the matched directive instance.
    #[test]
    fn signal_view_child_with_read_token() {
        let mut q = query("el", SignalQueryKind::ViewChild, "['ref']");
        q.read_source = Some("ElementRef".into());
        let r = compile_signal_members(&[], &[], &[], &[], &[q]);
        let vq = r.view_query_prop.as_ref().expect("viewQuery");
        assert!(vq.contains("\u{0275}\u{0275}viewQuerySignal(ctx.el, ['ref'], 5, ElementRef)"));
    }

    /// Mixing view + content signal queries on the same class produces
    /// both functions, each with its own advance counts. View queries
    /// must not leak into the content function (and vice versa) — they
    /// share an LView slot pool but live in distinct create/refresh
    /// blocks.
    #[test]
    fn mixed_view_and_content_queries_split_into_two_props() {
        let queries = vec![
            query("child", SignalQueryKind::ViewChild, "C"),
            query("projected", SignalQueryKind::ContentChild, "P"),
        ];
        let r = compile_signal_members(&[], &[], &[], &[], &queries);
        let vq = r.view_query_prop.expect("viewQuery");
        let cq = r.content_queries_prop.expect("contentQueries");
        assert!(vq.contains("ctx.child"));
        assert!(!vq.contains("ctx.projected"));
        assert!(cq.contains("ctx.projected"));
        assert!(!cq.contains("ctx.child"));
    }

    /// `model<T>()` desugars to a paired SignalBased input + change
    /// event. The OUTPUTS map must be keyed by the model's class
    /// property name (matching `instance.<field>`, where the runtime
    /// finds the model signal to subscribe to) — keying by the public
    /// event name (`<field>Change`) sends the runtime looking for a
    /// non-existent `instance.<field>Change` and the two-way binding
    /// silently drops on every emission.
    #[test]
    fn signal_model_outputs_keyed_by_class_property() {
        let spec = SignalModelSpec {
            property_name: "active".into(),
            alias: None,
            is_required: false,
        };
        let r = compile_signal_members(&[], &[], &[], &[spec], &[]);
        assert!(
            r.outputs_entries
                .iter()
                .any(|e| e == "active: 'activeChange'"),
            "expected outputs keyed by class prop, got: {:?}",
            r.outputs_entries
        );
        assert!(
            !r.outputs_entries
                .iter()
                .any(|e| e.starts_with("activeChange:")),
            "outputs map must NOT be keyed by the public event name: {:?}",
            r.outputs_entries
        );
    }
}
