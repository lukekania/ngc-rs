//! AOT codegen for `@Directive` decorators.
//!
//! Generates `ɵfac` (factory) and `ɵdir` (`ɵɵdefineDirective`) static fields.
//!
//! ## Example
//! ```text
//! // Input:
//! @Directive({ selector: '[appHighlight]', standalone: true })
//! export class HighlightDirective {}
//!
//! // Output:
//! export class HighlightDirective {
//!   static ɵfac = function HighlightDirective_Factory(t: any) { return new (t || HighlightDirective)(); };
//!   static ɵdir = ɵɵdefineDirective({ type: HighlightDirective, selectors: [['', 'appHighlight', '']], standalone: true });
//! }
//! ```

use std::collections::BTreeSet;

use ngc_diagnostics::NgcResult;

use crate::codegen::IvyOutput;
use crate::extract::ExtractedDirective;
use crate::factory_codegen;
use crate::selector;

/// Generate Ivy output for a `@Directive` decorator.
pub fn generate_directive_ivy(extracted: &ExtractedDirective) -> NgcResult<IvyOutput> {
    let name = &extracted.class_name;
    let mut ivy_imports = BTreeSet::new();

    ivy_imports.insert("\u{0275}\u{0275}defineDirective".to_string());

    // Generate factory with DI
    let (factory_code, inject_imports) =
        factory_codegen::generate_factory(name, &extracted.constructor_params);
    for imp in inject_imports {
        ivy_imports.insert(imp);
    }

    // Build ɵdir definition
    let mut props = Vec::new();
    props.push(format!("type: {name}"));

    if let Some(ref sel) = extracted.selector {
        props.push(format!("selectors: {}", selector::parse_selector(sel)));
    }

    if let Some(ref inputs_src) = extracted.inputs_source {
        props.push(format!("inputs: {inputs_src}"));
    }

    if let Some(ref outputs_src) = extracted.outputs_source {
        props.push(format!("outputs: {outputs_src}"));
    }

    if let Some(ref export_as) = extracted.export_as {
        let parts: Vec<&str> = export_as.split(',').map(|s| s.trim()).collect();
        let arr = parts
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", ");
        props.push(format!("exportAs: [{arr}]"));
    }

    if extracted.standalone {
        props.push("standalone: true".to_string());
    }

    let define_code = format!(
        "static \u{0275}dir = \u{0275}\u{0275}defineDirective({{ {} }})",
        props.join(", ")
    );

    Ok(IvyOutput {
        factory_code,
        static_fields: vec![define_code],
        child_template_functions: Vec::new(),
        ivy_imports,
        consts: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{ConstructorParam, DecoratorCommon};

    fn make_directive(
        class_name: &str,
        selector: Option<&str>,
        standalone: bool,
    ) -> ExtractedDirective {
        ExtractedDirective {
            class_name: class_name.to_string(),
            selector: selector.map(|s| s.to_string()),
            standalone,
            inputs_source: None,
            outputs_source: None,
            export_as: None,
            constructor_params: Vec::new(),
            common: DecoratorCommon {
                decorator_span: (0, 0),
                class_body_start: 0,
                angular_core_import_span: None,
                other_angular_core_imports: Vec::new(),
            },
        }
    }

    #[test]
    fn test_directive_basic() {
        let extracted = make_directive("HighlightDirective", Some("[appHighlight]"), true);
        let output = generate_directive_ivy(&extracted).unwrap();
        assert!(output.factory_code.contains("HighlightDirective_Factory"));
        assert!(output.static_fields[0].contains("\u{0275}\u{0275}defineDirective"));
        assert!(output.static_fields[0].contains("type: HighlightDirective"));
        assert!(output.static_fields[0].contains("selectors: [['', 'appHighlight', '']]"));
        assert!(output.static_fields[0].contains("standalone: true"));
    }

    #[test]
    fn test_directive_element_selector() {
        let extracted = make_directive("MyComp", Some("my-comp"), false);
        let output = generate_directive_ivy(&extracted).unwrap();
        assert!(output.static_fields[0].contains("selectors: [['my-comp']]"));
        assert!(!output.static_fields[0].contains("standalone"));
    }

    #[test]
    fn test_directive_no_selector() {
        let extracted = make_directive("AbstractDir", None, false);
        let output = generate_directive_ivy(&extracted).unwrap();
        assert!(!output.static_fields[0].contains("selectors"));
    }

    #[test]
    fn test_directive_with_deps() {
        let mut extracted = make_directive("MyDir", Some("[myDir]"), true);
        extracted.constructor_params = vec![ConstructorParam {
            type_name: Some("ElementRef".to_string()),
            inject_token: None,
            optional: false,
            self_: false,
            skip_self: false,
            host: false,
        }];
        let output = generate_directive_ivy(&extracted).unwrap();
        assert!(output
            .factory_code
            .contains("\u{0275}\u{0275}inject(ElementRef)"));
    }

    #[test]
    fn test_directive_with_export_as() {
        let mut extracted = make_directive("MyDir", Some("[myDir]"), true);
        extracted.export_as = Some("myDir".to_string());
        let output = generate_directive_ivy(&extracted).unwrap();
        assert!(output.static_fields[0].contains("exportAs: [\"myDir\"]"));
    }
}
