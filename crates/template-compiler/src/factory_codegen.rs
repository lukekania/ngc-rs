//! Shared factory function generation for Angular AOT compilation.
//!
//! Generates `static ɵfac = function ClassName_Factory(t: any) { ... }` code
//! with dependency injection support via `ɵɵinject()` calls.

use std::collections::BTreeSet;

use crate::extract::ConstructorParam;

/// Generate a factory function with DI inject calls for constructor parameters.
///
/// Returns the factory code string and a set of Ivy import symbols needed.
pub fn generate_factory(
    class_name: &str,
    params: &[ConstructorParam],
) -> (String, BTreeSet<String>) {
    let mut imports = BTreeSet::new();

    if params.is_empty() {
        let code = format!(
            "static \u{0275}fac = function {class_name}_Factory(t: any) {{ return new (t || {class_name})(); }}"
        );
        return (code, imports);
    }

    let args: Vec<String> = params
        .iter()
        .filter_map(|p| {
            let token = p.inject_token.as_deref().or(p.type_name.as_deref())?;

            imports.insert("\u{0275}\u{0275}inject".to_string());

            let mut flags = 0u32;
            if p.optional {
                flags |= 8;
            }
            if p.self_ {
                flags |= 2;
            }
            if p.skip_self {
                flags |= 4;
            }
            if p.host {
                flags |= 1;
            }

            if flags != 0 {
                Some(format!("\u{0275}\u{0275}inject({token}, {flags})"))
            } else {
                Some(format!("\u{0275}\u{0275}inject({token})"))
            }
        })
        .collect();

    let args_str = args.join(", ");
    let code = format!(
        "static \u{0275}fac = function {class_name}_Factory(t: any) {{ return new (t || {class_name})({args_str}); }}"
    );

    (code, imports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_factory_no_params() {
        let (code, imports) = generate_factory("MyService", &[]);
        assert!(code.contains("MyService_Factory"));
        assert!(code.contains("new (t || MyService)()"));
        assert!(imports.is_empty());
    }

    #[test]
    fn test_factory_with_type_param() {
        let params = vec![ConstructorParam {
            type_name: Some("HttpClient".to_string()),
            inject_token: None,
            optional: false,
            self_: false,
            skip_self: false,
            host: false,
        }];
        let (code, imports) = generate_factory("DataService", &params);
        assert!(code.contains("\u{0275}\u{0275}inject(HttpClient)"));
        assert!(imports.contains("\u{0275}\u{0275}inject"));
    }

    #[test]
    fn test_factory_with_inject_token() {
        let params = vec![ConstructorParam {
            type_name: Some("string".to_string()),
            inject_token: Some("'API_URL'".to_string()),
            optional: false,
            self_: false,
            skip_self: false,
            host: false,
        }];
        let (code, _) = generate_factory("MyService", &params);
        // @Inject token takes precedence over type
        assert!(code.contains("\u{0275}\u{0275}inject('API_URL')"));
    }

    #[test]
    fn test_factory_with_optional_flag() {
        let params = vec![ConstructorParam {
            type_name: Some("SomeDep".to_string()),
            inject_token: None,
            optional: true,
            self_: false,
            skip_self: false,
            host: false,
        }];
        let (code, _) = generate_factory("MyService", &params);
        assert!(code.contains("\u{0275}\u{0275}inject(SomeDep, 8)"));
    }

    #[test]
    fn test_factory_with_multiple_flags() {
        let params = vec![ConstructorParam {
            type_name: Some("SomeDep".to_string()),
            inject_token: None,
            optional: true,
            self_: false,
            skip_self: true,
            host: false,
        }];
        let (code, _) = generate_factory("MyService", &params);
        // optional=8, skip_self=4 → 12
        assert!(code.contains("\u{0275}\u{0275}inject(SomeDep, 12)"));
    }

    #[test]
    fn test_factory_multiple_params() {
        let params = vec![
            ConstructorParam {
                type_name: Some("HttpClient".to_string()),
                inject_token: None,
                optional: false,
                self_: false,
                skip_self: false,
                host: false,
            },
            ConstructorParam {
                type_name: Some("Router".to_string()),
                inject_token: None,
                optional: false,
                self_: false,
                skip_self: false,
                host: false,
            },
        ];
        let (code, _) = generate_factory("AuthService", &params);
        assert!(code.contains("\u{0275}\u{0275}inject(HttpClient), \u{0275}\u{0275}inject(Router)"));
    }
}
