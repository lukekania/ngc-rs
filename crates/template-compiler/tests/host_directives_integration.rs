//! End-to-end integration test for `hostDirectives` composition (issue #57).
//!
//! Compiles a host `@Component` whose decorator declares a composed
//! `@Directive` via `hostDirectives` — including input remapping and an
//! output rename — and verifies the rewritten source contains the
//! `ɵɵHostDirectivesFeature(...)` feature call inside the `defineComponent`
//! features array.
//!
//! Angular's runtime reads this feature at directive-instantiation time:
//! - It instantiates the composed directive on the host element so that
//!   directive's own `host` listeners and bindings (e.g. `(click)` handlers,
//!   `[class.foo]` bindings) run on the host.
//! - It applies the `inputs` remapping so the host's bindings (`<x-host
//!   [aliasedInput]="...">`) reach the composed directive's private fields.
//! - It applies the `outputs` remapping so emissions from the composed
//!   directive surface on the host under the renamed event name.
//!
//! Without `ɵɵHostDirectivesFeature`, none of that wiring exists — the
//! composed directive class is silently dropped, host bindings on it never
//! run, and inputs never reach it. This test guards the complete codegen
//! shape that makes that wiring work at runtime.

use std::path::PathBuf;

use ngc_template_compiler::compile_component;

const FIXTURE_SOURCE: &str = r#"
import { Component, Directive, EventEmitter, HostBinding, HostListener, Input, Output } from '@angular/core';

@Directive({
  selector: '[appActivatable]',
  standalone: true,
})
export class ActivatableDirective {
  @Input() active = false;
  @Output() activated = new EventEmitter<void>();

  @HostBinding('class.is-active') get classBinding() { return this.active; }
  @HostListener('click') onClick() { this.activated.emit(); }
}

@Component({
  selector: 'app-host',
  standalone: true,
  hostDirectives: [
    {
      directive: ActivatableDirective,
      inputs: ['active: highlighted'],
      outputs: ['activated: hostActivated'],
    },
  ],
  template: '<span>host</span>',
})
export class HostComponent {}
"#;

#[test]
fn host_directives_composition_emits_feature_call() {
    let compiled = compile_component(FIXTURE_SOURCE, &PathBuf::from("host.component.ts"))
        .expect("component should compile");

    assert!(compiled.compiled, "compile_component should rewrite source");
    assert!(
        !compiled.jit_fallback,
        "hostDirectives must not trigger JIT fallback"
    );

    let out = &compiled.source;

    // Feature call must wrap the hostDirectives array. Without it, the
    // composed directive class is dropped at runtime — no instantiation,
    // no host bindings, no input remapping.
    assert!(
        out.contains("\u{0275}\u{0275}HostDirectivesFeature(["),
        "expected ɵɵHostDirectivesFeature wrapper:\n{out}"
    );

    // The composed directive class reference must appear inside the array.
    // This is what keeps tree-shaking from dropping ActivatableDirective.
    assert!(
        out.contains("directive: ActivatableDirective"),
        "expected composed directive class reference preserved:\n{out}"
    );

    // Input/output remapping strings must reach the runtime as flat pairs.
    // Angular's `bindingArrayToMap` reads `bindings[i]` (publicName) and
    // `bindings[i+1]` (privateName) — leaving the decorator's colon syntax
    // intact would make it read a single key with `undefined` value, silently
    // dropping the remapping at runtime.
    assert!(
        out.contains("'active', 'highlighted'"),
        "input remapping must reach the runtime as a flat pair:\n{out}"
    );
    assert!(
        out.contains("'activated', 'hostActivated'"),
        "output remapping must reach the runtime as a flat pair:\n{out}"
    );
    assert!(
        !out.contains("'active: highlighted'"),
        "raw colon-syntax string must not survive to the runtime:\n{out}"
    );

    // Symbol must be imported so the bundler can resolve it and the runtime
    // call can dispatch.
    assert!(
        out.contains("\u{0275}\u{0275}HostDirectivesFeature"),
        "feature symbol must be imported from @angular/core:\n{out}"
    );

    // Rewritten source must remain valid TS that ts-transform can lower to JS.
    let js = ngc_ts_transform::transform_source(out, "host.component.ts")
        .expect("compiled source should parse through ts-transform");
    assert!(
        js.contains("\u{0275}\u{0275}HostDirectivesFeature"),
        "feature call must survive ts-transform:\n{js}"
    );
}
