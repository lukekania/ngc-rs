//! AOT codegen for `@NgModule` decorators.
//!
//! Generates `ɵfac` (factory), `ɵmod` (`ɵɵdefineNgModule`), and `ɵinj`
//! (`ɵɵdefineInjector`) static fields.
//!
//! ## Example
//! ```text
//! // Input:
//! @NgModule({ declarations: [AppComponent], imports: [CommonModule], bootstrap: [AppComponent] })
//! export class AppModule {}
//!
//! // Output:
//! export class AppModule {
//!   static ɵfac = function AppModule_Factory(t: any) { return new (t || AppModule)(); };
//!   static ɵmod = ɵɵdefineNgModule({ type: AppModule, declarations: [AppComponent], imports: [CommonModule], bootstrap: [AppComponent] });
//!   static ɵinj = ɵɵdefineInjector({ imports: [CommonModule] });
//! }
//! ```

use std::collections::BTreeSet;

use ngc_diagnostics::NgcResult;

use crate::codegen::IvyOutput;
use crate::extract::ExtractedNgModule;
use crate::factory_codegen;

/// Generate Ivy output for an `@NgModule` decorator.
pub fn generate_ng_module_ivy(extracted: &ExtractedNgModule) -> NgcResult<IvyOutput> {
    let name = &extracted.class_name;
    let mut ivy_imports = BTreeSet::new();

    ivy_imports.insert("\u{0275}\u{0275}defineNgModule".to_string());
    ivy_imports.insert("\u{0275}\u{0275}defineInjector".to_string());

    // Generate factory (NgModules typically have no constructor params)
    let (factory_code, inject_imports) = factory_codegen::generate_factory(name, &[]);
    for imp in inject_imports {
        ivy_imports.insert(imp);
    }

    // Build ɵmod = ɵɵdefineNgModule({...})
    let mut mod_props = Vec::new();
    mod_props.push(format!("type: {name}"));

    if let Some(ref declarations) = extracted.declarations_source {
        mod_props.push(format!("declarations: {declarations}"));
    }
    if let Some(ref imports) = extracted.imports_source {
        mod_props.push(format!("imports: {imports}"));
    }
    if let Some(ref exports) = extracted.exports_source {
        mod_props.push(format!("exports: {exports}"));
    }
    if let Some(ref bootstrap) = extracted.bootstrap_source {
        mod_props.push(format!("bootstrap: {bootstrap}"));
    }

    let mod_code = format!(
        "static \u{0275}mod = \u{0275}\u{0275}defineNgModule({{ {} }})",
        mod_props.join(", ")
    );

    // Build ɵinj = ɵɵdefineInjector({...})
    let mut inj_props = Vec::new();

    if let Some(ref providers) = extracted.providers_source {
        inj_props.push(format!("providers: {providers}"));
    }
    if let Some(ref imports) = extracted.imports_source {
        inj_props.push(format!("imports: {imports}"));
    }

    let inj_code = if inj_props.is_empty() {
        "static \u{0275}inj = \u{0275}\u{0275}defineInjector({})".to_string()
    } else {
        format!(
            "static \u{0275}inj = \u{0275}\u{0275}defineInjector({{ {} }})",
            inj_props.join(", ")
        )
    };

    Ok(IvyOutput {
        factory_code,
        static_fields: vec![mod_code, inj_code],
        child_template_functions: Vec::new(),
        ivy_imports,
        consts: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::DecoratorCommon;

    fn make_ng_module(class_name: &str) -> ExtractedNgModule {
        ExtractedNgModule {
            class_name: class_name.to_string(),
            declarations_source: None,
            imports_source: None,
            exports_source: None,
            providers_source: None,
            bootstrap_source: None,
            common: DecoratorCommon {
                decorator_span: (0, 0),
                class_body_start: 0,
                angular_core_import_span: None,
                other_angular_core_imports: Vec::new(),
            },
        }
    }

    #[test]
    fn test_ng_module_basic() {
        let mut extracted = make_ng_module("AppModule");
        extracted.declarations_source = Some("[AppComponent]".to_string());
        extracted.imports_source = Some("[CommonModule]".to_string());
        extracted.bootstrap_source = Some("[AppComponent]".to_string());

        let output = generate_ng_module_ivy(&extracted).unwrap();
        assert!(output.factory_code.contains("AppModule_Factory"));

        // Should have two static fields: ɵmod and ɵinj
        assert_eq!(output.static_fields.len(), 2);
        assert!(output.static_fields[0].contains("\u{0275}\u{0275}defineNgModule"));
        assert!(output.static_fields[0].contains("type: AppModule"));
        assert!(output.static_fields[0].contains("declarations: [AppComponent]"));
        assert!(output.static_fields[0].contains("imports: [CommonModule]"));
        assert!(output.static_fields[0].contains("bootstrap: [AppComponent]"));

        assert!(output.static_fields[1].contains("\u{0275}\u{0275}defineInjector"));
        assert!(output.static_fields[1].contains("imports: [CommonModule]"));
    }

    #[test]
    fn test_ng_module_empty() {
        let extracted = make_ng_module("EmptyModule");
        let output = generate_ng_module_ivy(&extracted).unwrap();
        assert_eq!(output.static_fields.len(), 2);
        assert!(output.static_fields[1].contains("\u{0275}\u{0275}defineInjector({})"));
    }

    #[test]
    fn test_ng_module_with_providers() {
        let mut extracted = make_ng_module("AppModule");
        extracted.providers_source = Some("[AuthService]".to_string());
        extracted.imports_source = Some("[HttpClientModule]".to_string());

        let output = generate_ng_module_ivy(&extracted).unwrap();
        assert!(output.static_fields[1].contains("providers: [AuthService]"));
        assert!(output.static_fields[1].contains("imports: [HttpClientModule]"));
    }

    #[test]
    fn test_ng_module_ivy_imports() {
        let extracted = make_ng_module("AppModule");
        let output = generate_ng_module_ivy(&extracted).unwrap();
        assert!(output
            .ivy_imports
            .contains("\u{0275}\u{0275}defineNgModule"));
        assert!(output
            .ivy_imports
            .contains("\u{0275}\u{0275}defineInjector"));
    }
}
