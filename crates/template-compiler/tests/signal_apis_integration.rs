//! End-to-end integration test for signal-based authoring APIs (issue #55).
//!
//! Compiles a `@Component` whose class fields use Angular 17+'s signal
//! authoring APIs — `input()`, `input.required()`, `input` with a
//! transform, `output()`, `model()`, and the four query factories
//! (`viewChild`, `viewChildren`, `contentChild`, `contentChildren`) —
//! then verifies the rewritten source carries the matching Ivy runtime
//! shapes:
//!
//! * `inputs: { foo: [<flags>, 'public', 'foo', transform?], ... }`
//!   where bit 0 of `<flags>` is `InputFlags.SignalBased` and bit 1 is
//!   `InputFlags.HasDecoratorInputTransform`.
//! * `outputs: { changed: 'changed', ... }` — outputs are pure name
//!   maps, no flags. `model()` adds a paired `<name>Change` entry.
//! * `viewQuery: function(rf, ctx) { ... ɵɵviewQuerySignal(...) ... ɵɵqueryAdvance(); }`
//! * `contentQueries: function(rf, ctx, directiveIndex) { ... ɵɵcontentQuerySignal(...) ... }`
//!
//! These are the exact shapes Angular's runtime reads — flipping bits,
//! omitting the signal write-through path, or skipping `queryAdvance()`
//! all silently break signal authoring at runtime, so the assertions
//! pin every required piece.

use std::path::PathBuf;

use ngc_template_compiler::compile_component;

const FIXTURE_SOURCE: &str = r#"
import {
  Component,
  input,
  output,
  model,
  viewChild,
  viewChildren,
  contentChild,
  contentChildren,
  ElementRef,
} from '@angular/core';

function trimString(v: string): string {
  return v.trim();
}

@Component({
  selector: 'app-x',
  standalone: true,
  template: '<span>x</span>',
})
export class XComponent {
  // Signal inputs — plain, required, aliased, transformed.
  name = input<string>('default');
  required = input.required<number>();
  aliased = input<string>('', { alias: 'pub' });
  trimmed = input<string>('', { transform: trimString });

  // Signal output.
  changed = output<string>();

  // Signal model — desugars to input + `valueChange` output.
  value = model<string>('initial');

  // Signal queries — view + content, single + plural.
  v = viewChild<string>('ref');
  vs = viewChildren(SomeCmp);
  c = contentChild<SomeDir>(SomeDir, { read: ElementRef });
  cs = contentChildren(SomeDir);
}
"#;

#[test]
fn signal_apis_reach_runtime_with_correct_shape() {
    let compiled = compile_component(FIXTURE_SOURCE, &PathBuf::from("x.component.ts"))
        .expect("component should compile");

    assert!(
        compiled.compiled,
        "compile_component should rewrite signal-API source"
    );
    assert!(
        !compiled.jit_fallback,
        "signal-API components must not fall back to JIT"
    );

    let out = &compiled.source;

    // Plain `input()` — SignalBased flag (bit 0) only.
    assert!(
        out.contains("name: [1, 'name', 'name']"),
        "expected plain input to set SignalBased flag, got:\n{out}"
    );

    // `input.required()` keeps the same SignalBased flag — required is
    // not a separate runtime flag.
    assert!(
        out.contains("required: [1, 'required', 'required']"),
        "expected required input to set SignalBased flag, got:\n{out}"
    );

    // Alias surfaces in array position 1 (publicName), property name in
    // position 2.
    assert!(
        out.contains("aliased: [1, 'pub', 'aliased']"),
        "expected aliased input to keep public name in position 1, got:\n{out}"
    );

    // Transform sets BOTH SignalBased (bit 0) and
    // HasDecoratorInputTransform (bit 1) → flags = 3 — and rides along
    // as the 4th array element so the runtime can call it.
    assert!(
        out.contains("trimmed: [3, 'trimmed', 'trimmed', trimString]"),
        "expected transform input to carry function ref + flag bit 1, got:\n{out}"
    );

    // Output → identity entry in the outputs map.
    assert!(
        out.contains("changed: 'changed'"),
        "expected output to land in outputs map, got:\n{out}"
    );

    // Model emits a paired input + `<name>Change` output.
    assert!(
        out.contains("value: [1, 'value', 'value']"),
        "expected model input entry, got:\n{out}"
    );
    assert!(
        out.contains("valueChange: 'valueChange'"),
        "expected model paired Change output, got:\n{out}"
    );

    // Signal queries dispatch to `ɵɵviewQuerySignal` / `ɵɵcontentQuerySignal`
    // — never the legacy `ɵɵviewQuery` / `ɵɵcontentQuery` for these.
    // The `ctx.<prop>` target in arg 1 (or arg 2 for content) is what
    // lets the runtime write into the WritableSignal slot directly.
    // `viewChild('ref')` wraps the bare-string predicate in an array
    // — runtime treats `'ref'` (string) as a `ProviderToken` and
    // `['ref']` (array) as a template-ref selector.
    assert!(
        out.contains("\u{0275}\u{0275}viewQuerySignal(ctx.v, ['ref']"),
        "expected viewQuerySignal create call for `v` with wrapped predicate, got:\n{out}"
    );
    assert!(
        out.contains("\u{0275}\u{0275}viewQuerySignal(ctx.vs"),
        "expected viewQuerySignal create call for `vs`, got:\n{out}"
    );
    assert!(
        out.contains("\u{0275}\u{0275}contentQuerySignal(directiveIndex, ctx.c"),
        "expected contentQuerySignal create call for `c`, got:\n{out}"
    );
    assert!(
        out.contains("\u{0275}\u{0275}contentQuerySignal(directiveIndex, ctx.cs"),
        "expected contentQuerySignal create call for `cs`, got:\n{out}"
    );

    // Each signal query needs a `ɵɵqueryAdvance()` in the update block
    // so the next query reads from the right LView slot. Without these
    // the second/third query would resolve against the first's slot.
    assert!(
        out.contains("\u{0275}\u{0275}queryAdvance();"),
        "expected ɵɵqueryAdvance update calls, got:\n{out}"
    );

    // Read token must reach the runtime as the trailing argument so
    // the runtime resolves an ElementRef rather than the matched
    // directive instance.
    assert!(
        out.contains("ElementRef"),
        "expected ElementRef read token preserved:\n{out}"
    );

    // Symbols must be added to the @angular/core import set so the
    // bundler can resolve them and the runtime calls dispatch.
    for symbol in [
        "\u{0275}\u{0275}viewQuerySignal",
        "\u{0275}\u{0275}contentQuerySignal",
        "\u{0275}\u{0275}queryAdvance",
    ] {
        assert!(
            out.contains(symbol),
            "expected {symbol} to be imported, got:\n{out}"
        );
    }

    // Rewritten source must lower cleanly through ts-transform — the
    // pipeline that turns the rewritten TS into the .mjs the bundler
    // consumes.
    let js = ngc_ts_transform::transform_source(out, "x.component.ts")
        .expect("compiled source should parse through ts-transform");
    assert!(
        js.contains("viewQuerySignal"),
        "signal-query call must survive ts-transform:\n{js}"
    );
}
