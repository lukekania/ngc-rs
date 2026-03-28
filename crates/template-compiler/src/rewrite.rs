use ngc_diagnostics::NgcResult;

use crate::codegen::IvyOutput;
use crate::extract::ExtractedComponent;

/// Rewrite a TypeScript source string to replace the `@Component` decorator
/// with Ivy static metadata.
///
/// Removes the decorator, updates the `@angular/core` import to include Ivy
/// runtime symbols, and inserts `static ɵfac` and `static ɵcmp` inside the
/// class body.
pub fn rewrite_source(
    source: &str,
    component: &ExtractedComponent,
    ivy_output: &IvyOutput,
) -> NgcResult<String> {
    // Collect all edits as (offset, operation) and apply in reverse order
    let mut result = source.to_string();

    // 1. Insert static fields inside the class body (after the opening `{`)
    let class_body_start = component.class_body_start as usize;
    if class_body_start < result.len() {
        let insert_pos = class_body_start + 1; // after `{`
        let mut insertion = String::new();
        insertion.push('\n');
        insertion.push_str("  ");
        insertion.push_str(&ivy_output.factory_code);
        insertion.push_str(";\n");
        insertion.push_str("  ");
        insertion.push_str(&ivy_output.define_component_code);
        insertion.push_str(";\n");
        result.insert_str(insert_pos, &insertion);
    }

    // 2. Remove the decorator (must be done after insertion to avoid offset issues
    //    since decorator comes before the class body)
    // Actually, since the decorator is BEFORE the class body, and we inserted INTO
    // the class body, the decorator's original spans are still valid in `source`
    // but shifted in `result`. Let's recalculate.
    //
    // Better approach: work on the original source positions and compute a diff.
    // For correctness, let's rebuild from scratch.

    let mut result = String::new();

    // Build the new @angular/core import line
    let mut ivy_symbols: Vec<&str> = ivy_output.ivy_imports.iter().map(|s| s.as_str()).collect();
    // Add back any non-Component imports from the original
    for imp in &component.other_angular_core_imports {
        if !ivy_symbols.contains(&imp.as_str()) {
            ivy_symbols.push(imp);
        }
    }
    ivy_symbols.sort();
    let new_import = format!(
        "import {{ {} }} from '@angular/core';",
        ivy_symbols.join(", ")
    );

    // Prepend child template functions
    for child_fn in &ivy_output.child_template_functions {
        result.push_str(child_fn);
        result.push('\n');
    }

    // Process the source line by line, but use spans for precision
    let decorator_start = component.decorator_span.0 as usize;
    let decorator_end = component.decorator_span.1 as usize;

    // Part 1: everything before the decorator, with import rewriting
    let before_decorator = &source[..decorator_start];
    if let Some((import_start, import_end)) = component.angular_core_import_span {
        let import_start = import_start as usize;
        let import_end = import_end as usize;
        // Before the import
        result.push_str(&source[..import_start]);
        // Rewritten import
        result.push_str(&new_import);
        // Between import end and decorator start
        result.push_str(&source[import_end..decorator_start]);
    } else {
        // No @angular/core import found — prepend new import and keep everything
        result.push_str(&new_import);
        result.push('\n');
        result.push_str(before_decorator);
    }

    // Part 2: skip the decorator — find where the decorator ends
    // The decorator span may include trailing whitespace/newline; skip it
    let mut after_decorator_pos = decorator_end;
    while after_decorator_pos < source.len()
        && (source.as_bytes()[after_decorator_pos] == b'\n'
            || source.as_bytes()[after_decorator_pos] == b'\r')
    {
        after_decorator_pos += 1;
    }

    // Part 3: the class declaration (without the decorator) — insert static fields
    let rest = &source[after_decorator_pos..];

    // Find the class body opening `{` in the remaining source
    let class_body_offset = component.class_body_start as usize;
    if class_body_offset >= after_decorator_pos && class_body_offset < source.len() {
        let relative_body_start = class_body_offset - after_decorator_pos;
        // Before class body opening
        result.push_str(&rest[..relative_body_start + 1]); // include the `{`
                                                           // Insert static fields
        result.push('\n');
        result.push_str("  ");
        result.push_str(&ivy_output.factory_code);
        result.push_str(";\n");
        result.push_str("  ");
        result.push_str(&ivy_output.define_component_code);
        result.push_str(";\n");
        // Rest of the class and file
        result.push_str(&rest[relative_body_start + 1..]);
    } else {
        result.push_str(rest);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::IvyOutput;
    use crate::extract::ExtractedComponent;
    use std::collections::BTreeSet;

    fn make_component() -> ExtractedComponent {
        ExtractedComponent {
            class_name: "AppComponent".to_string(),
            selector: "app-root".to_string(),
            template: Some("<h1>Hello</h1>".to_string()),
            template_url: None,
            standalone: true,
            imports_source: None,
            imports_identifiers: Vec::new(),
            decorator_span: (45, 130),
            class_body_start: 162,
            export_keyword_start: Some(131),
            class_keyword_start: 138,
            angular_core_import_span: Some((0, 44)),
            other_angular_core_imports: Vec::new(),
        }
    }

    fn make_ivy_output() -> IvyOutput {
        IvyOutput {
            factory_code: "static \u{0275}fac = function AppComponent_Factory(t: any) { return new (t || AppComponent)(); }".to_string(),
            define_component_code: "static \u{0275}cmp = \u{0275}\u{0275}defineComponent({\n    type: AppComponent\n  })".to_string(),
            child_template_functions: Vec::new(),
            ivy_imports: BTreeSet::from([
                "\u{0275}\u{0275}defineComponent".to_string(),
                "\u{0275}\u{0275}element".to_string(),
            ]),
        }
    }

    #[test]
    fn test_rewrite_removes_decorator() {
        let source = "import { Component } from '@angular/core';\n\n@Component({\n  selector: 'app-root',\n  template: '<h1>Hello</h1>'\n})\nexport class AppComponent {\n  title = 'app';\n}\n";
        let component = make_component();
        let ivy = make_ivy_output();
        let result = rewrite_source(source, &component, &ivy).expect("should rewrite");
        assert!(!result.contains("@Component"));
        assert!(result.contains("\u{0275}\u{0275}defineComponent"));
    }

    #[test]
    fn test_rewrite_updates_imports() {
        let source = "import { Component } from '@angular/core';\n\n@Component({\n  selector: 'app-root',\n  template: '<h1>Hello</h1>'\n})\nexport class AppComponent {\n  title = 'app';\n}\n";
        let component = make_component();
        let ivy = make_ivy_output();
        let result = rewrite_source(source, &component, &ivy).expect("should rewrite");
        // Should have Ivy imports instead of Component
        assert!(result.contains("\u{0275}\u{0275}defineComponent"));
        assert!(result.contains("\u{0275}\u{0275}element"));
        assert!(!result.contains("import { Component }"));
    }
}
