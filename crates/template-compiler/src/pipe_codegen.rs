//! AOT codegen for `@Pipe` decorators.
//!
//! Generates `ɵfac` (factory) and `ɵpipe` (`ɵɵdefinePipe`) static fields.
//!
//! ## Example
//! ```text
//! // Input:
//! @Pipe({ name: 'dateFormat', standalone: true })
//! export class DateFormatPipe implements PipeTransform { ... }
//!
//! // Output:
//! export class DateFormatPipe {
//!   static ɵfac = function DateFormatPipe_Factory(t: any) { return new (t || DateFormatPipe)(); };
//!   static ɵpipe = ɵɵdefinePipe({ name: 'dateFormat', type: DateFormatPipe, pure: true, standalone: true });
//! }
//! ```

use std::collections::BTreeSet;

use ngc_diagnostics::NgcResult;

use crate::codegen::IvyOutput;
use crate::extract::ExtractedPipe;
use crate::factory_codegen;

/// Generate Ivy output for a `@Pipe` decorator.
pub fn generate_pipe_ivy(extracted: &ExtractedPipe) -> NgcResult<IvyOutput> {
    let name = &extracted.class_name;
    let mut ivy_imports = BTreeSet::new();

    ivy_imports.insert("\u{0275}\u{0275}definePipe".to_string());

    // Generate factory with DI
    let (factory_code, inject_imports) =
        factory_codegen::generate_factory(name, &extracted.constructor_params);
    for imp in inject_imports {
        ivy_imports.insert(imp);
    }

    // Build ɵpipe definition
    let mut props = Vec::new();
    props.push(format!("name: '{}'", extracted.pipe_name));
    props.push(format!("type: {name}"));

    // Angular pipes are pure by default
    let pure = extracted.pure.unwrap_or(true);
    props.push(format!("pure: {pure}"));

    if extracted.standalone {
        props.push("standalone: true".to_string());
    }

    let define_code = format!(
        "static \u{0275}pipe = \u{0275}\u{0275}definePipe({{ {} }})",
        props.join(", ")
    );

    Ok(IvyOutput {
        factory_code,
        static_fields: vec![define_code],
        child_template_functions: Vec::new(),
        ivy_imports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::DecoratorCommon;

    fn make_pipe(
        pipe_name: &str,
        class_name: &str,
        pure: Option<bool>,
        standalone: bool,
    ) -> ExtractedPipe {
        ExtractedPipe {
            class_name: class_name.to_string(),
            pipe_name: pipe_name.to_string(),
            pure,
            standalone,
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
    fn test_pipe_basic() {
        let extracted = make_pipe("dateFormat", "DateFormatPipe", None, true);
        let output = generate_pipe_ivy(&extracted).unwrap();
        assert!(output.factory_code.contains("DateFormatPipe_Factory"));
        assert!(output.static_fields[0].contains("\u{0275}\u{0275}definePipe"));
        assert!(output.static_fields[0].contains("name: 'dateFormat'"));
        assert!(output.static_fields[0].contains("type: DateFormatPipe"));
        assert!(output.static_fields[0].contains("pure: true"));
        assert!(output.static_fields[0].contains("standalone: true"));
    }

    #[test]
    fn test_pipe_impure() {
        let extracted = make_pipe("async", "AsyncPipe", Some(false), true);
        let output = generate_pipe_ivy(&extracted).unwrap();
        assert!(output.static_fields[0].contains("pure: false"));
    }

    #[test]
    fn test_pipe_not_standalone() {
        let extracted = make_pipe("myPipe", "MyPipe", None, false);
        let output = generate_pipe_ivy(&extracted).unwrap();
        assert!(!output.static_fields[0].contains("standalone"));
    }
}
