//! AOT codegen for `@Injectable` decorators.
//!
//! Generates `ɵfac` (factory) and `ɵprov` (`ɵɵdefineInjectable`) static fields.
//!
//! ## Example
//! ```text
//! // Input:
//! @Injectable({ providedIn: 'root' })
//! export class AuthService { constructor(private http: HttpClient) {} }
//!
//! // Output:
//! export class AuthService {
//!   static ɵfac = function AuthService_Factory(t: any) { return new (t || AuthService)(ɵɵinject(HttpClient)); };
//!   static ɵprov = ɵɵdefineInjectable({ token: AuthService, factory: AuthService.ɵfac, providedIn: 'root' });
//! }
//! ```

use std::collections::BTreeSet;

use ngc_diagnostics::NgcResult;

use crate::codegen::IvyOutput;
use crate::extract::ExtractedInjectable;
use crate::factory_codegen;

/// Generate Ivy output for an `@Injectable` decorator.
pub fn generate_injectable_ivy(extracted: &ExtractedInjectable) -> NgcResult<IvyOutput> {
    let name = &extracted.class_name;
    let mut ivy_imports = BTreeSet::new();

    ivy_imports.insert("\u{0275}\u{0275}defineInjectable".to_string());

    // Generate factory with DI
    let (factory_code, inject_imports) =
        factory_codegen::generate_factory(name, &extracted.constructor_params);
    for imp in inject_imports {
        ivy_imports.insert(imp);
    }

    // Determine the factory expression for defineInjectable
    let factory_expr = if extracted.use_factory.is_some()
        || extracted.use_class.is_some()
        || extracted.use_value.is_some()
        || extracted.use_existing.is_some()
    {
        // Provider override — use the specified factory instead of ɵfac
        if let Some(ref use_factory) = extracted.use_factory {
            use_factory.clone()
        } else if let Some(ref use_class) = extracted.use_class {
            format!("() => new {use_class}()")
        } else if let Some(ref use_value) = extracted.use_value {
            format!("() => {use_value}")
        } else if let Some(ref use_existing) = extracted.use_existing {
            ivy_imports.insert("\u{0275}\u{0275}inject".to_string());
            format!("function() {{ return \u{0275}\u{0275}inject({use_existing}); }}")
        } else {
            format!("{name}.\u{0275}fac")
        }
    } else {
        format!("{name}.\u{0275}fac")
    };

    // Build ɵprov definition
    let mut props = Vec::new();
    props.push(format!("token: {name}"));
    props.push(format!("factory: {factory_expr}"));

    if let Some(ref provided_in) = extracted.provided_in {
        props.push(format!("providedIn: {provided_in}"));
    }

    let define_code = format!(
        "static \u{0275}prov = \u{0275}\u{0275}defineInjectable({{ {} }})",
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

    fn make_injectable(
        class_name: &str,
        provided_in: Option<&str>,
        params: Vec<ConstructorParam>,
    ) -> ExtractedInjectable {
        ExtractedInjectable {
            class_name: class_name.to_string(),
            provided_in: provided_in.map(|s| s.to_string()),
            use_factory: None,
            use_class: None,
            use_value: None,
            use_existing: None,
            constructor_params: params,
            common: DecoratorCommon {
                decorator_span: (0, 0),
                class_body_start: 0,
                angular_core_import_span: None,
                other_angular_core_imports: Vec::new(),
            },
        }
    }

    #[test]
    fn test_injectable_basic() {
        let extracted = make_injectable("AuthService", Some("'root'"), vec![]);
        let output = generate_injectable_ivy(&extracted).unwrap();
        assert!(output.factory_code.contains("AuthService_Factory"));
        assert!(output.static_fields[0].contains("\u{0275}\u{0275}defineInjectable"));
        assert!(output.static_fields[0].contains("token: AuthService"));
        assert!(output.static_fields[0].contains("factory: AuthService.\u{0275}fac"));
        assert!(output.static_fields[0].contains("providedIn: 'root'"));
    }

    #[test]
    fn test_injectable_with_deps() {
        let params = vec![ConstructorParam {
            type_name: Some("HttpClient".to_string()),
            inject_token: None,
            optional: false,
            self_: false,
            skip_self: false,
            host: false,
        }];
        let extracted = make_injectable("DataService", Some("'root'"), params);
        let output = generate_injectable_ivy(&extracted).unwrap();
        assert!(output
            .factory_code
            .contains("\u{0275}\u{0275}inject(HttpClient)"));
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}inject"));
    }

    #[test]
    fn test_injectable_no_provided_in() {
        let extracted = make_injectable("MyService", None, vec![]);
        let output = generate_injectable_ivy(&extracted).unwrap();
        assert!(!output.static_fields[0].contains("providedIn"));
    }

    #[test]
    fn test_injectable_use_existing() {
        let mut extracted = make_injectable("MyService", Some("'root'"), vec![]);
        extracted.use_existing = Some("OtherService".to_string());
        let output = generate_injectable_ivy(&extracted).unwrap();
        assert!(output.static_fields[0].contains("\u{0275}\u{0275}inject(OtherService)"));
    }
}
