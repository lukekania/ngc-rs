//! End-to-end integration test for `@if (expr; as alias)` / `@else if (...)`
//! alias binding (issue #166).
//!
//! `#165` stopped the build from breaking on the `; as alias` syntax but the
//! alias was not actually bound at runtime — references inside the body
//! compiled to `ctx.<alias>` on the parent component context, which is
//! `undefined` for any component that doesn't happen to carry such a field.
//!
//! This file pins the corrected codegen end-to-end: a component whose
//! template uses `@if (item(); as it) { {{ it.name }} ... }` (the
//! `DetailComponent` pattern) plus `@else if (...; as ...)` and a nested
//! `@switch` inside an aliased `@if` (the `HttpClientComponent` pattern)
//! must run through `compile_component` and emit code that:
//!
//!   * passes the truthy expression value as `ɵɵconditional`'s second arg,
//!   * uses the alias as the inner template function's parameter, and
//!   * resolves alias references from nested scopes via `ɵɵnextContext()`.

use std::path::PathBuf;

use ngc_template_compiler::compile_component;

const DETAIL_FIXTURE: &str = r#"
import { Component, input } from '@angular/core';

interface Item {
  id: string;
  name: string;
  summary: string;
}

@Component({
  selector: 'app-routing-detail',
  standalone: true,
  template: `
    @if (item(); as it) {
      <p>{{ it.name }}</p>
      <p>{{ it.summary }}</p>
    } @else {
      <p>Item not found.</p>
    }
  `,
})
export class DetailComponent {
  readonly item = input<Item | null>(null);
}
"#;

const ELSE_IF_FIXTURE: &str = r#"
import { Component } from '@angular/core';

@Component({
  selector: 'app-x',
  standalone: true,
  template: `
    @if (a(); as ax) { {{ ax.foo }} }
    @else if (b(); as bx) { {{ bx.bar }} }
  `,
})
export class XComponent {
  a() { return null; }
  b() { return null; }
}
"#;

const NESTED_FIXTURE: &str = r#"
import { Component } from '@angular/core';

interface State { k: string; v: string; }

@Component({
  selector: 'app-y',
  standalone: true,
  template: `
    @if (state(); as s) {
      @switch (s.k) {
        @case ('a') { <p>{{ s.v }}</p> }
      }
    }
  `,
})
export class YComponent {
  state(): State | null { return null; }
}
"#;

#[test]
fn if_alias_emits_runtime_correct_codegen() {
    let compiled = compile_component(DETAIL_FIXTURE, &PathBuf::from("detail.component.ts"))
        .expect("component should compile");

    assert!(
        compiled.compiled,
        "compile_component must rewrite the @if-alias source"
    );
    assert!(
        !compiled.jit_fallback,
        "an `@if (expr; as alias)` body must not trigger JIT fallback"
    );

    let out = &compiled.source;

    // 1. The aliased branch's child template takes the alias as its `_ctx`
    //    parameter (`it`) — that is the runtime value Angular's
    //    `ɵɵconditional` delivers.
    assert!(
        out.contains("function DetailComponent_Conditional_0_Template(rf, it)"),
        "alias must be the inner template's _ctx parameter:\n{out}"
    );

    // 2. Body interpolations read the parameter directly, NOT `ctx.it` on
    //    the parent (which has no `it` field).
    assert!(
        out.contains("\u{0275}\u{0275}textInterpolate(it.name);"),
        "alias body must reference the alias as a local, not ctx.<alias>:\n{out}"
    );
    assert!(
        !out.contains("ctx.it."),
        "alias body must NOT fall back to ctx.<alias>.<field>:\n{out}"
    );

    // 3. The `ɵɵconditional` call passes the truthy expression value as its
    //    second arg so the matching template's `_ctx` carries it. The
    //    expression is re-evaluated (same shape Angular's own compiler
    //    emits) — `item()` appears on both sides of the ternary chain.
    assert!(
        out.contains("\u{0275}\u{0275}conditional(ctx.item() ? ")
            && out.contains(", ctx.item() ? ctx.item() : null);"),
        "ɵɵconditional must receive the alias value as its second arg:\n{out}"
    );

    // 4. The `@else` branch keeps the generic `_ctx` parameter (Angular's
    //    grammar does not allow `; as alias` on `@else`).
    assert!(
        out.contains("function DetailComponent_ConditionalElse_1_Template(rf, _ctx)"),
        "@else branch must keep _ctx param when there is no alias:\n{out}"
    );
}

#[test]
fn else_if_alias_binds_per_branch_independently() {
    let compiled = compile_component(ELSE_IF_FIXTURE, &PathBuf::from("x.component.ts"))
        .expect("component should compile");
    let out = &compiled.source;

    assert!(
        out.contains("function XComponent_Conditional_0_Template(rf, ax)"),
        "@if branch's alias must be its template param:\n{out}"
    );
    assert!(
        out.contains("function XComponent_ConditionalElseIf_1_Template(rf, bx)"),
        "@else if branch's alias must be its template param:\n{out}"
    );
    // Both branches contribute to the alias-value chain in the same order
    // as the test chain — when branch N matches, the matching template's
    // _ctx is the N-th branch's expression value.
    assert!(
        out.contains(
            "\u{0275}\u{0275}conditional(ctx.a() ? 0 : ctx.b() ? 1 : -1, \
             ctx.a() ? ctx.a() : ctx.b() ? ctx.b() : null);"
        ),
        "alias-value chain must mirror the test chain branch-for-branch:\n{out}"
    );
}

#[test]
fn nested_scope_reads_outer_if_alias_via_next_context() {
    let compiled = compile_component(NESTED_FIXTURE, &PathBuf::from("y.component.ts"))
        .expect("component should compile");
    let out = &compiled.source;

    // The @switch case body is nested two levels deep (root → @if → @switch
    // case). The @if's embedded view holds the alias `s` as its `_ctx`, so
    // a single `ɵɵnextContext()` from the case body retrieves it; the
    // navigation prelude must emit that binding before the body's
    // instructions.
    assert!(
        out.contains("const s = \u{0275}\u{0275}nextContext();"),
        "nested case must extract the outer alias via ɵɵnextContext():\n{out}"
    );
    // Interpolation inside the @switch case reads the local `s`, never
    // `ctx.s` on the component.
    assert!(
        out.contains("\u{0275}\u{0275}textInterpolate(s.v);"),
        "nested alias references stay unprefixed:\n{out}"
    );
    assert!(
        !out.contains("ctx.s."),
        "nested scope must NOT read ctx.<alias>.<field>:\n{out}"
    );
}
