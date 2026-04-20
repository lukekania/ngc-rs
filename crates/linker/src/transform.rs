//! AST walking, declaration detection, and span-based replacement.
//!
//! Parses an npm module's JavaScript source with oxc, finds all `ɵɵngDeclare*`
//! call expressions, transforms each one, and applies the replacements to the
//! original source text.

use std::path::Path;

use ngc_diagnostics::{NgcError, NgcResult};
use oxc_allocator::Allocator;
use oxc_ast::ast::{CallExpression, Expression, Program, Statement};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

use crate::module_registry::ModuleRegistry;
use crate::{class_metadata, component, directive, factory, injectable, injector, ng_module, pipe};

/// A single text replacement to apply to the source.
#[derive(Debug)]
struct Replacement {
    /// Start byte offset in the original source.
    start: u32,
    /// End byte offset in the original source.
    end: u32,
    /// The replacement text.
    text: String,
}

/// The kind of `ɵɵngDeclare*` call found in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclareKind {
    Factory,
    Injectable,
    Injector,
    NgModule,
    Pipe,
    Directive,
    Component,
    ClassMetadata,
}

/// Detect the `ɵɵngDeclare*` variant from a callee name suffix.
fn detect_declare_kind(name: &str) -> Option<DeclareKind> {
    // Handle both bare names and member-expression property names
    if name.ends_with("ngDeclareFactory") {
        Some(DeclareKind::Factory)
    } else if name.ends_with("ngDeclareInjectable") {
        Some(DeclareKind::Injectable)
    } else if name.ends_with("ngDeclareInjector") {
        Some(DeclareKind::Injector)
    } else if name.ends_with("ngDeclareNgModule") {
        Some(DeclareKind::NgModule)
    } else if name.ends_with("ngDeclarePipe") {
        Some(DeclareKind::Pipe)
    } else if name.ends_with("ngDeclareDirective") {
        Some(DeclareKind::Directive)
    } else if name.ends_with("ngDeclareComponent") {
        Some(DeclareKind::Component)
    } else if name.ends_with("ngDeclareClassMetadata") {
        Some(DeclareKind::ClassMetadata)
    } else {
        None
    }
}

/// Extract the callee name from a call expression.
///
/// Handles both bare identifiers (`ɵɵngDeclareFactory(...)`) and member
/// expressions (`i0.ɵɵngDeclareFactory(...)`).
fn get_callee_name(call: &CallExpression<'_>) -> Option<String> {
    match &call.callee {
        Expression::Identifier(id) => Some(id.name.to_string()),
        Expression::StaticMemberExpression(member) => Some(member.property.name.to_string()),
        _ => None,
    }
}

/// Extract the `ngImport` alias (e.g., `"i0"`) from the callee of a declare call.
///
/// For `i0.ɵɵngDeclareFactory(...)`, returns `"i0"`.
/// For bare `ɵɵngDeclareFactory(...)`, returns `None`.
fn get_ng_import_alias(call: &CallExpression<'_>) -> Option<String> {
    if let Expression::StaticMemberExpression(member) = &call.callee {
        if let Expression::Identifier(id) = &member.object {
            return Some(id.name.to_string());
        }
    }
    None
}

/// Transform a single npm module source by linking all `ɵɵngDeclare*` calls.
///
/// Returns the transformed source, or `None` if no declarations were found.
/// Any `ɵɵngDeclareNgModule` calls encountered are also registered in
/// `registry` so the post-link flatten pass can expand them.
pub fn link_source(
    source: &str,
    file_path: &Path,
    registry: &ModuleRegistry,
) -> NgcResult<Option<String>> {
    let alloc = Allocator::default();
    let parsed = Parser::new(&alloc, source, SourceType::mjs()).parse();

    if !parsed.errors.is_empty() {
        return Err(NgcError::LinkerError {
            path: file_path.to_path_buf(),
            message: format!("parse error: {}", parsed.errors[0]),
        });
    }

    let mut replacements = Vec::new();
    collect_replacements(
        &parsed.program,
        source,
        file_path,
        registry,
        &mut replacements,
    )?;

    if replacements.is_empty() {
        return Ok(None);
    }

    // Sort by start offset descending so we can apply replacements from end to start
    replacements.sort_by_key(|e| std::cmp::Reverse(e.start));

    let mut result = source.to_string();
    for r in &replacements {
        result.replace_range(r.start as usize..r.end as usize, &r.text);
    }

    Ok(Some(result))
}

/// Walk the program AST and collect all replacements for `ɵɵngDeclare*` calls.
fn collect_replacements(
    program: &Program<'_>,
    source: &str,
    file_path: &Path,
    registry: &ModuleRegistry,
    replacements: &mut Vec<Replacement>,
) -> NgcResult<()> {
    for stmt in &program.body {
        visit_statement(stmt, source, file_path, registry, replacements)?;
    }
    Ok(())
}

/// Visit a statement, looking for `ɵɵngDeclare*` call expressions anywhere within.
fn visit_statement(
    stmt: &Statement<'_>,
    source: &str,
    file_path: &Path,
    registry: &ModuleRegistry,
    replacements: &mut Vec<Replacement>,
) -> NgcResult<()> {
    match stmt {
        Statement::ExpressionStatement(expr_stmt) => {
            visit_expression(
                &expr_stmt.expression,
                source,
                file_path,
                registry,
                replacements,
            )?;
        }
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(init) = &declarator.init {
                    visit_expression(init, source, file_path, registry, replacements)?;
                }
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(ref decl) = export.declaration {
                match decl {
                    oxc_ast::ast::Declaration::VariableDeclaration(var_decl) => {
                        for declarator in &var_decl.declarations {
                            if let Some(init) = &declarator.init {
                                visit_expression(init, source, file_path, registry, replacements)?;
                            }
                        }
                    }
                    oxc_ast::ast::Declaration::ClassDeclaration(class) => {
                        visit_class_body(class, source, file_path, registry, replacements)?;
                    }
                    _ => {}
                }
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let oxc_ast::ast::ExportDefaultDeclarationKind::ClassDeclaration(class) =
                &export.declaration
            {
                visit_class_body(class, source, file_path, registry, replacements)?;
            }
        }
        Statement::ClassDeclaration(class) => {
            visit_class_body(class, source, file_path, registry, replacements)?;
        }
        _ => {}
    }
    Ok(())
}

/// Visit class body looking for static property definitions and static blocks
/// with `ɵɵngDeclare*` initializers.
fn visit_class_body(
    class: &oxc_ast::ast::Class<'_>,
    source: &str,
    file_path: &Path,
    registry: &ModuleRegistry,
    replacements: &mut Vec<Replacement>,
) -> NgcResult<()> {
    for element in &class.body.body {
        match element {
            oxc_ast::ast::ClassElement::PropertyDefinition(prop) => {
                if let Some(ref init) = prop.value {
                    visit_expression(init, source, file_path, registry, replacements)?;
                }
            }
            oxc_ast::ast::ClassElement::StaticBlock(block) => {
                // static { this.ɵfac = i0.ɵɵngDeclareFactory({...}); }
                for stmt in &block.body {
                    visit_statement(stmt, source, file_path, registry, replacements)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Visit an expression, looking for `ɵɵngDeclare*` call expressions.
fn visit_expression(
    expr: &Expression<'_>,
    source: &str,
    file_path: &Path,
    registry: &ModuleRegistry,
    replacements: &mut Vec<Replacement>,
) -> NgcResult<()> {
    match expr {
        Expression::CallExpression(call) => {
            if let Some(replacement) =
                try_transform_declare_call(call, source, file_path, registry)?
            {
                replacements.push(replacement);
            }
        }
        Expression::AssignmentExpression(assign) => {
            visit_expression(&assign.right, source, file_path, registry, replacements)?;
        }
        Expression::SequenceExpression(seq) => {
            for expr in &seq.expressions {
                visit_expression(expr, source, file_path, registry, replacements)?;
            }
        }
        Expression::ClassExpression(class) => {
            visit_class_body(class, source, file_path, registry, replacements)?;
        }
        _ => {}
    }
    Ok(())
}

/// Try to transform a call expression if it's a `ɵɵngDeclare*` call.
///
/// Returns a `Replacement` if the call was recognized and transformed.
fn try_transform_declare_call(
    call: &CallExpression<'_>,
    source: &str,
    file_path: &Path,
    registry: &ModuleRegistry,
) -> NgcResult<Option<Replacement>> {
    let callee_name = match get_callee_name(call) {
        Some(name) => name,
        None => return Ok(None),
    };

    let kind = match detect_declare_kind(&callee_name) {
        Some(k) => k,
        None => return Ok(None),
    };

    // The first argument should be an object expression
    let obj = match call.arguments.first() {
        Some(arg) => match &arg {
            oxc_ast::ast::Argument::ObjectExpression(obj) => obj.as_ref(),
            _ => {
                return Err(NgcError::LinkerError {
                    path: file_path.to_path_buf(),
                    message: format!("{callee_name}: expected object literal argument"),
                });
            }
        },
        None => {
            return Err(NgcError::LinkerError {
                path: file_path.to_path_buf(),
                message: format!("{callee_name}: missing argument"),
            });
        }
    };

    let ng_import = get_ng_import_alias(call).unwrap_or_default();

    let replacement_text = match kind {
        DeclareKind::Factory => factory::transform(obj, source, &ng_import)?,
        DeclareKind::Injectable => injectable::transform(obj, source, &ng_import)?,
        DeclareKind::Injector => injector::transform(obj, source, &ng_import)?,
        DeclareKind::NgModule => ng_module::transform(obj, source, &ng_import, registry)?,
        DeclareKind::Pipe => pipe::transform(obj, source, &ng_import)?,
        DeclareKind::Directive => directive::transform(obj, source, &ng_import, file_path)?,
        DeclareKind::Component => component::transform(obj, source, &ng_import, file_path)?,
        DeclareKind::ClassMetadata => class_metadata::transform(obj, source, &ng_import)?,
    };

    let span = call.span();
    Ok(Some(Replacement {
        start: span.start,
        end: span.end,
        text: replacement_text,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_detect_declare_kind() {
        assert_eq!(
            detect_declare_kind("\u{0275}\u{0275}ngDeclareFactory"),
            Some(DeclareKind::Factory)
        );
        assert_eq!(
            detect_declare_kind("\u{0275}\u{0275}ngDeclareInjectable"),
            Some(DeclareKind::Injectable)
        );
        assert_eq!(
            detect_declare_kind("\u{0275}\u{0275}ngDeclareComponent"),
            Some(DeclareKind::Component)
        );
        assert_eq!(detect_declare_kind("someOtherFunction"), None);
    }

    #[test]
    fn test_no_declarations_returns_none() {
        let source = "export class Foo { bar() {} }";
        let registry = ModuleRegistry::new();
        let result = link_source(source, &PathBuf::from("test.mjs"), &registry).unwrap();
        assert!(result.is_none());
    }
}
