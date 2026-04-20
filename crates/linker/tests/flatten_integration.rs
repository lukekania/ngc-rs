//! End-to-end integration for `link_modules`.
//!
//! Exercises the full three-pass pipeline (npm link → register → flatten) on
//! a synthetic module graph that mimics the shape ngc-rs produces when
//! building a standalone Angular app that imports `ReactiveFormsModule`.
//! Verifies that the component's `dependencies` array ends up as a flat list
//! of directive class names — no factory wrappers, no module identifiers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ngc_linker::link_modules;

const FORMS_MJS: &str = "import * as i0 from '@angular/core';\n\
\n\
class FormGroupDirective {}\n\
class FormControlName {}\n\
class NgControlStatus {}\n\
class InternalFormsSharedModule {}\n\
class ReactiveFormsModule {}\n\
\n\
InternalFormsSharedModule.\u{0275}mod = i0.\u{0275}\u{0275}ngDeclareNgModule({ minVersion: \"14.0.0\", version: \"17.0.0\", ngImport: i0, type: InternalFormsSharedModule, exports: [NgControlStatus] });\n\
ReactiveFormsModule.\u{0275}mod = i0.\u{0275}\u{0275}ngDeclareNgModule({ minVersion: \"14.0.0\", version: \"17.0.0\", ngImport: i0, type: ReactiveFormsModule, exports: [InternalFormsSharedModule, FormGroupDirective, FormControlName] });\n\
\n\
export { ReactiveFormsModule, FormGroupDirective, FormControlName, NgControlStatus };";

const PROJECT_COMPONENT: &str = "import { ReactiveFormsModule } from '@angular/forms';\n\
import { MyStandaloneDir } from './my-dir';\n\
\n\
class DialogComponent {}\n\
DialogComponent.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: DialogComponent, selectors: [[\"app-dialog\"]], standalone: true, dependencies: [ReactiveFormsModule, MyStandaloneDir], template: function DialogComponent_Template() {} });\n\
\n\
export { DialogComponent };";

#[test]
fn link_modules_flattens_reactive_forms_module_imports() {
    let mut modules = HashMap::new();
    modules.insert(
        PathBuf::from("/project/node_modules/@angular/forms/fesm2022/forms.mjs"),
        FORMS_MJS.to_string(),
    );
    modules.insert(
        PathBuf::from("/project/src/app/dialog.component.ts"),
        PROJECT_COMPONENT.to_string(),
    );

    let stats = link_modules(&mut modules, Path::new("/project")).expect("link_modules");

    // The forms.mjs file had ɵɵngDeclare* calls and should have been rewritten.
    assert_eq!(stats.files_scanned, 1);
    assert_eq!(stats.files_linked, 1);
    // Pass A re-scans the now-linked forms module and the raw project component.
    assert!(stats.modules_registered >= 2, "{stats:?}");
    assert_eq!(stats.components_flattened, 1);

    // The component's dependencies array is now a flat directive list with
    // ReactiveFormsModule expanded transitively (InternalFormsSharedModule →
    // NgControlStatus, plus ReactiveFormsModule's own three directives).
    let dialog = modules
        .get(Path::new("/project/src/app/dialog.component.ts"))
        .expect("dialog present");
    assert!(
        dialog.contains(
            "dependencies: [NgControlStatus, FormGroupDirective, FormControlName, MyStandaloneDir]"
        ),
        "unexpected dependencies array: {dialog}"
    );
    // The factory wrapper must not appear anywhere.
    assert!(
        !dialog.contains("getComponentDepsFactory"),
        "factory wrapper leaked: {dialog}"
    );
    // The npm forms module must have been rewritten to ɵɵdefineNgModule.
    let forms = modules
        .get(Path::new(
            "/project/node_modules/@angular/forms/fesm2022/forms.mjs",
        ))
        .expect("forms present");
    assert!(!forms.contains("\u{0275}\u{0275}ngDeclare"));
    assert!(forms.contains("\u{0275}\u{0275}defineNgModule"));
}

#[test]
fn link_modules_noop_when_no_modules_or_components() {
    let mut modules = HashMap::new();
    modules.insert(
        PathBuf::from("/project/src/plain.ts"),
        "export function add(a, b) { return a + b; }".to_string(),
    );

    let stats = link_modules(&mut modules, Path::new("/project")).expect("link_modules");
    assert_eq!(stats.files_linked, 0);
    assert_eq!(stats.modules_registered, 0);
    assert_eq!(stats.components_flattened, 0);
}
