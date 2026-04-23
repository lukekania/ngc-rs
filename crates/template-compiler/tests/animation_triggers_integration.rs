//! End-to-end integration test for Angular animation trigger template syntax.
//!
//! Compiles an `@Component` decorator that uses `@angular/animations`
//! `trigger('fade', [...])` in the `animations` array and references the
//! trigger in the template via `[@fade]="state"` and
//! `(@fade.done)="onDone($event)"`. Verifies that the rewritten source
//! contains the expected Ivy instructions:
//!
//!   - `ɵɵproperty('@fade', ctx.state)` in the update block
//!   - `ɵɵlistener('@fade.done', ...)` in the creation block
//!   - `ɵɵlistener('@fade.start', ...)` for the `.start` phase
//!
//! Angular's runtime routes `@`-prefixed property names to the animation
//! renderer and dispatches the listener when the matching state transition
//! reaches the given phase, so emitting these instructions is what makes
//! `trigger(...)` work against the real `@angular/animations` package.

use std::path::PathBuf;

use ngc_template_compiler::compile_component;

const FIXTURE_SOURCE: &str = r#"
import { Component } from '@angular/core';
import { trigger, state, style, transition, animate } from '@angular/animations';

@Component({
  selector: 'app-fade',
  standalone: true,
  template: `
    <div
      [@fade]="state"
      (@fade.done)="onDone($event)"
      (@fade.start)="onStart($event)">
      fade me
    </div>
  `,
  animations: [
    trigger('fade', [
      state('visible', style({ opacity: 1 })),
      state('hidden', style({ opacity: 0 })),
      transition('visible <=> hidden', animate('200ms ease-in-out')),
    ]),
  ],
})
export class FadeComponent {
  state: 'visible' | 'hidden' = 'visible';
  onDone(_event: unknown) {}
  onStart(_event: unknown) {}
}
"#;

#[test]
fn animation_trigger_template_compiles_to_ivy_instructions() {
    let compiled = compile_component(FIXTURE_SOURCE, &PathBuf::from("fade.component.ts"))
        .expect("component should compile");

    assert!(compiled.compiled, "compile_component should rewrite source");
    assert!(
        !compiled.jit_fallback,
        "animation trigger syntax must not trigger JIT fallback"
    );

    let rewritten = &compiled.source;

    assert!(
        rewritten.contains("\u{0275}\u{0275}property('@fade', ctx.state)"),
        "expected ɵɵproperty('@fade', ctx.state) in rewritten source:\n{rewritten}"
    );
    assert!(
        rewritten.contains("\u{0275}\u{0275}listener('@fade.done'"),
        "expected ɵɵlistener('@fade.done', ...) in rewritten source:\n{rewritten}"
    );
    assert!(
        rewritten.contains("\u{0275}\u{0275}listener('@fade.start'"),
        "expected ɵɵlistener('@fade.start', ...) in rewritten source:\n{rewritten}"
    );
    assert!(
        rewritten.contains("ctx.onDone($event)"),
        "listener body should forward $event to ctx.onDone:\n{rewritten}"
    );
    assert!(
        rewritten.contains("ctx.onStart($event)"),
        "listener body should forward $event to ctx.onStart:\n{rewritten}"
    );

    assert!(
        rewritten.contains("trigger('fade'"),
        "original animations array should be preserved for Angular's runtime:\n{rewritten}"
    );
}
