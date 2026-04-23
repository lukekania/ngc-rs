use std::path::Path;

use ngc_diagnostics::{NgcError, NgcResult};
use pest_derive::Parser;

use crate::ast::TemplateNode;

#[derive(Parser)]
#[grammar = "grammar/angular.pest"]
struct AngularTemplateParser;

/// Parse an Angular template string into an AST.
///
/// Uses a pest grammar to parse the HTML-like template syntax including
/// elements, interpolation, bindings, control flow, and pipes.
pub fn parse_template(template: &str, file_path: &Path) -> NgcResult<Vec<TemplateNode>> {
    use pest::Parser;

    let pairs = AngularTemplateParser::parse(Rule::template, template).map_err(|e| {
        NgcError::TemplateParseError {
            path: file_path.to_path_buf(),
            message: e.to_string(),
        }
    })?;

    let mut nodes = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::template {
            for inner in pair.into_inner() {
                if inner.as_rule() != Rule::EOI {
                    if let Some(node) = parse_node(inner)? {
                        nodes.push(node);
                    }
                }
            }
        }
    }

    Ok(nodes)
}

fn parse_node(pair: pest::iterators::Pair<Rule>) -> NgcResult<Option<TemplateNode>> {
    match pair.as_rule() {
        Rule::element | Rule::void_element | Rule::paired_element => Ok(Some(parse_element(pair)?)),
        Rule::text => {
            let value = pair.as_str().to_string();
            if value.trim().is_empty() {
                return Ok(None);
            }
            Ok(Some(TemplateNode::Text(crate::ast::TextNode { value })))
        }
        Rule::html_comment => Ok(None), // Skip HTML comments
        Rule::interpolation => Ok(Some(parse_interpolation(pair)?)),
        Rule::if_block => Ok(Some(parse_if_block(pair)?)),
        Rule::for_block => Ok(Some(parse_for_block(pair)?)),
        Rule::switch_block => Ok(Some(parse_switch_block(pair)?)),
        Rule::let_block => Ok(Some(parse_let_block(pair)?)),
        Rule::node => {
            let inner = pair.into_inner().next();
            match inner {
                Some(p) => parse_node(p),
                None => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

fn parse_element(pair: pest::iterators::Pair<Rule>) -> NgcResult<TemplateNode> {
    let rule = pair.as_rule();
    let mut inner = pair.into_inner();

    match rule {
        Rule::void_element => {
            let tag = inner
                .next()
                .map(|p| p.as_str().to_string())
                .unwrap_or_default();
            let attributes = parse_attributes(&mut inner)?;
            Ok(TemplateNode::Element(crate::ast::ElementNode {
                tag,
                attributes,
                children: Vec::new(),
                is_void: true,
            }))
        }
        Rule::paired_element => {
            let open_tag = inner.next();
            let (tag, attributes) = if let Some(open) = open_tag {
                let mut open_inner = open.into_inner();
                let tag = open_inner
                    .next()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                let attrs = parse_attributes(&mut open_inner)?;
                (tag, attrs)
            } else {
                (String::new(), Vec::new())
            };

            let mut children = Vec::new();
            for child_pair in inner {
                match child_pair.as_rule() {
                    Rule::close_tag => break,
                    _ => {
                        if let Some(node) = parse_node(child_pair)? {
                            children.push(node);
                        }
                    }
                }
            }

            Ok(TemplateNode::Element(crate::ast::ElementNode {
                tag,
                attributes,
                children,
                is_void: false,
            }))
        }
        Rule::element => {
            // element is a choice between void_element and paired_element
            if let Some(child) = inner.next() {
                parse_element(child)
            } else {
                Ok(TemplateNode::Text(crate::ast::TextNode {
                    value: String::new(),
                }))
            }
        }
        _ => Ok(TemplateNode::Text(crate::ast::TextNode {
            value: String::new(),
        })),
    }
}

fn parse_attributes(
    pairs: &mut pest::iterators::Pairs<Rule>,
) -> NgcResult<Vec<crate::ast::TemplateAttribute>> {
    let mut attrs = Vec::new();
    for pair in pairs {
        match pair.as_rule() {
            Rule::event_binding => {
                let mut inner = pair.into_inner();
                let name = inner
                    .next()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                let handler = inner
                    .next()
                    .and_then(|p| p.into_inner().next())
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                attrs.push(crate::ast::TemplateAttribute::Event { name, handler });
            }
            Rule::property_binding => {
                let mut inner = pair.into_inner();
                let name = inner
                    .next()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                let expression = inner
                    .next()
                    .and_then(|p| p.into_inner().next())
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                attrs.push(crate::ast::TemplateAttribute::Property { name, expression });
            }
            Rule::class_binding => {
                let mut inner = pair.into_inner();
                let class_name = inner
                    .next()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                let expression = inner
                    .next()
                    .and_then(|p| p.into_inner().next())
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                attrs.push(crate::ast::TemplateAttribute::ClassBinding {
                    class_name,
                    expression,
                });
            }
            Rule::style_binding => {
                let mut inner = pair.into_inner();
                let property = inner
                    .next()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                let expression = inner
                    .next()
                    .and_then(|p| p.into_inner().next())
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                attrs.push(crate::ast::TemplateAttribute::StyleBinding {
                    property,
                    expression,
                });
            }
            Rule::attr_binding => {
                let mut inner = pair.into_inner();
                let name = inner
                    .next()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                let expression = inner
                    .next()
                    .and_then(|p| p.into_inner().next())
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                attrs.push(crate::ast::TemplateAttribute::AttrBinding { name, expression });
            }
            Rule::two_way_binding => {
                let mut inner = pair.into_inner();
                let name = inner
                    .next()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                let expression = inner
                    .next()
                    .and_then(|p| p.into_inner().next())
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                attrs.push(crate::ast::TemplateAttribute::TwoWayBinding { name, expression });
            }
            Rule::structural_directive => {
                let mut inner = pair.into_inner();
                let name = inner
                    .next()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                let expression = inner
                    .next()
                    .and_then(|p| p.into_inner().next())
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                attrs.push(crate::ast::TemplateAttribute::StructuralDirective { name, expression });
            }
            Rule::ref_variable => {
                let mut inner = pair.into_inner();
                let name = inner
                    .next()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                let export_as = inner
                    .next()
                    .and_then(|p| p.into_inner().next())
                    .map(|p| p.as_str().to_string());
                attrs.push(crate::ast::TemplateAttribute::Reference { name, export_as });
            }
            Rule::static_attribute => {
                let mut inner = pair.into_inner();
                let name = inner
                    .next()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                let value = inner
                    .next()
                    .and_then(|p| p.into_inner().next())
                    .map(|p| p.as_str().to_string());
                attrs.push(crate::ast::TemplateAttribute::Static { name, value });
            }
            _ => {}
        }
    }
    Ok(attrs)
}

fn parse_interpolation(pair: pest::iterators::Pair<Rule>) -> NgcResult<TemplateNode> {
    let mut inner = pair.into_inner();
    // inner should be interp_expression
    let expr_pair = inner.next();

    let (expression, pipes) = if let Some(expr) = expr_pair {
        parse_interp_expression(expr)?
    } else {
        (String::new(), Vec::new())
    };

    Ok(TemplateNode::Interpolation(crate::ast::InterpolationNode {
        expression,
        pipes,
    }))
}

fn parse_interp_expression(
    pair: pest::iterators::Pair<Rule>,
) -> NgcResult<(String, Vec<crate::ast::PipeCall>)> {
    let mut inner = pair.into_inner();
    // interp_segment contains the full expression text (including balanced parens)
    let raw_expr = inner
        .next()
        .map(|p| p.as_str().trim().to_string())
        .unwrap_or_default();

    let mut pipes = Vec::new();
    for pipe_pair in inner {
        if pipe_pair.as_rule() == Rule::pipe_call {
            let mut pipe_inner = pipe_pair.into_inner();
            let name = pipe_inner
                .next()
                .map(|p| p.as_str().to_string())
                .unwrap_or_default();
            let args: Vec<String> = pipe_inner
                .filter(|p| p.as_rule() == Rule::pipe_arg)
                .map(|p| p.as_str().trim().to_string())
                .collect();
            pipes.push(crate::ast::PipeCall { name, args });
        }
    }

    Ok((raw_expr, pipes))
}

fn parse_if_block(pair: pest::iterators::Pair<Rule>) -> NgcResult<TemplateNode> {
    let mut inner = pair.into_inner();

    let condition = inner
        .next()
        .map(|p| extract_expression_text(p))
        .unwrap_or_default();

    let mut children = Vec::new();
    let mut else_if_branches = Vec::new();
    let mut else_branch = None;

    for p in inner {
        match p.as_rule() {
            Rule::else_if_block => {
                let mut ei_inner = p.into_inner();
                let ei_cond = ei_inner
                    .next()
                    .map(|p| extract_expression_text(p))
                    .unwrap_or_default();
                let mut ei_children = Vec::new();
                for child in ei_inner {
                    if let Some(node) = parse_node(child)? {
                        ei_children.push(node);
                    }
                }
                else_if_branches.push(crate::ast::ElseIfBranch {
                    condition: ei_cond,
                    children: ei_children,
                });
            }
            Rule::else_block => {
                let mut eb_children = Vec::new();
                for child in p.into_inner() {
                    if let Some(node) = parse_node(child)? {
                        eb_children.push(node);
                    }
                }
                else_branch = Some(eb_children);
            }
            _ => {
                if let Some(node) = parse_node(p)? {
                    children.push(node);
                }
            }
        }
    }

    Ok(TemplateNode::IfBlock(crate::ast::IfBlockNode {
        condition,
        children,
        else_if_branches,
        else_branch,
    }))
}

fn parse_for_block(pair: pest::iterators::Pair<Rule>) -> NgcResult<TemplateNode> {
    let mut inner = pair.into_inner();

    let for_vars = inner.next();
    let (item_name, iterable, track_expression) = if let Some(vars) = for_vars {
        let mut vars_inner = vars.into_inner();
        let item = vars_inner
            .next()
            .map(|p| p.as_str().to_string())
            .unwrap_or_default();
        let iter = vars_inner
            .next()
            .map(|p| p.as_str().trim().to_string())
            .unwrap_or_default();
        let track = vars_inner
            .next()
            .map(|p| p.as_str().trim().to_string())
            .unwrap_or_default();
        (item, iter, track)
    } else {
        (String::new(), String::new(), String::new())
    };

    let mut children = Vec::new();
    let mut empty_children = None;

    for p in inner {
        match p.as_rule() {
            Rule::empty_block => {
                let mut eb_children = Vec::new();
                for child in p.into_inner() {
                    if let Some(node) = parse_node(child)? {
                        eb_children.push(node);
                    }
                }
                empty_children = Some(eb_children);
            }
            _ => {
                if let Some(node) = parse_node(p)? {
                    children.push(node);
                }
            }
        }
    }

    Ok(TemplateNode::ForBlock(crate::ast::ForBlockNode {
        item_name,
        iterable,
        track_expression,
        children,
        empty_children,
    }))
}

fn parse_switch_block(pair: pest::iterators::Pair<Rule>) -> NgcResult<TemplateNode> {
    let mut inner = pair.into_inner();

    let expression = inner
        .next()
        .map(|p| extract_expression_text(p))
        .unwrap_or_default();

    let mut cases = Vec::new();
    let mut default_branch = None;

    for p in inner {
        match p.as_rule() {
            Rule::case_block => {
                let mut case_inner = p.into_inner();
                let case_expr = case_inner
                    .next()
                    .map(|p| extract_expression_text(p))
                    .unwrap_or_default();
                let mut case_children = Vec::new();
                for child in case_inner {
                    if let Some(node) = parse_node(child)? {
                        case_children.push(node);
                    }
                }
                cases.push(crate::ast::CaseBranch {
                    expression: case_expr,
                    children: case_children,
                });
            }
            Rule::default_block => {
                let mut db_children = Vec::new();
                for child in p.into_inner() {
                    if let Some(node) = parse_node(child)? {
                        db_children.push(node);
                    }
                }
                default_branch = Some(db_children);
            }
            _ => {}
        }
    }

    Ok(TemplateNode::SwitchBlock(crate::ast::SwitchBlockNode {
        expression,
        cases,
        default_branch,
    }))
}

/// Parse an `@let name = expression;` declaration.
fn parse_let_block(pair: pest::iterators::Pair<Rule>) -> NgcResult<TemplateNode> {
    let mut inner = pair.into_inner();

    let name = inner
        .next()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();

    let expression = inner
        .next()
        .map(|p| p.as_str().trim().to_string())
        .unwrap_or_default();

    Ok(TemplateNode::LetDeclaration(
        crate::ast::LetDeclarationNode { name, expression },
    ))
}

/// Extract the text content from an expression pair, handling nested rules.
fn extract_expression_text(pair: pest::iterators::Pair<Rule>) -> String {
    // For ctrl_expression and track_expression, the text comes from
    // the span of the full pair (including inner paren groups)
    pair.as_str().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::*;
    use std::path::PathBuf;

    fn parse(template: &str) -> Vec<TemplateNode> {
        parse_template(template, &PathBuf::from("test.html")).expect("should parse")
    }

    #[test]
    fn test_void_element() {
        let nodes = parse("<br />");
        assert_eq!(nodes.len(), 1);
        match &nodes[0] {
            TemplateNode::Element(e) => {
                assert_eq!(e.tag, "br");
                assert!(e.is_void);
            }
            _ => panic!("expected element"),
        }
    }

    #[test]
    fn test_paired_element_with_text() {
        let nodes = parse("<h1>Hello</h1>");
        assert_eq!(nodes.len(), 1);
        match &nodes[0] {
            TemplateNode::Element(e) => {
                assert_eq!(e.tag, "h1");
                assert!(!e.is_void);
                assert_eq!(e.children.len(), 1);
                match &e.children[0] {
                    TemplateNode::Text(t) => assert_eq!(t.value, "Hello"),
                    _ => panic!("expected text child"),
                }
            }
            _ => panic!("expected element"),
        }
    }

    #[test]
    fn test_interpolation() {
        let nodes = parse("{{ title }}");
        assert_eq!(nodes.len(), 1);
        match &nodes[0] {
            TemplateNode::Interpolation(i) => {
                assert_eq!(i.expression, "title");
                assert!(i.pipes.is_empty());
            }
            _ => panic!("expected interpolation"),
        }
    }

    #[test]
    fn test_interpolation_with_pipe() {
        let nodes = parse("{{ value | date:'short' }}");
        assert_eq!(nodes.len(), 1);
        match &nodes[0] {
            TemplateNode::Interpolation(i) => {
                assert_eq!(i.expression, "value");
                assert_eq!(i.pipes.len(), 1);
                assert_eq!(i.pipes[0].name, "date");
                assert_eq!(i.pipes[0].args, vec!["'short'"]);
            }
            _ => panic!("expected interpolation"),
        }
    }

    #[test]
    fn test_static_attribute() {
        let nodes = parse("<div class=\"container\"></div>");
        match &nodes[0] {
            TemplateNode::Element(e) => {
                assert_eq!(e.attributes.len(), 1);
                match &e.attributes[0] {
                    TemplateAttribute::Static { name, value } => {
                        assert_eq!(name, "class");
                        assert_eq!(value.as_deref(), Some("container"));
                    }
                    _ => panic!("expected static attribute"),
                }
            }
            _ => panic!("expected element"),
        }
    }

    #[test]
    fn test_property_binding() {
        let nodes = parse("<div [title]=\"expr\"></div>");
        match &nodes[0] {
            TemplateNode::Element(e) => match &e.attributes[0] {
                TemplateAttribute::Property { name, expression } => {
                    assert_eq!(name, "title");
                    assert_eq!(expression, "expr");
                }
                _ => panic!("expected property binding"),
            },
            _ => panic!("expected element"),
        }
    }

    #[test]
    fn test_event_binding() {
        let nodes = parse("<button (click)=\"onClick()\">Click</button>");
        match &nodes[0] {
            TemplateNode::Element(e) => {
                assert_eq!(e.tag, "button");
                match &e.attributes[0] {
                    TemplateAttribute::Event { name, handler } => {
                        assert_eq!(name, "click");
                        assert_eq!(handler, "onClick()");
                    }
                    _ => panic!("expected event binding"),
                }
            }
            _ => panic!("expected element"),
        }
    }

    #[test]
    fn test_if_block() {
        let nodes = parse("@if (show) { <p>Hello</p> }");
        assert_eq!(nodes.len(), 1);
        match &nodes[0] {
            TemplateNode::IfBlock(b) => {
                assert_eq!(b.condition, "show");
                assert_eq!(b.children.len(), 1);
                assert!(b.else_branch.is_none());
            }
            _ => panic!("expected if block"),
        }
    }

    #[test]
    fn test_if_else_block() {
        let nodes = parse("@if (show) { <p>Yes</p> } @else { <p>No</p> }");
        match &nodes[0] {
            TemplateNode::IfBlock(b) => {
                assert_eq!(b.condition, "show");
                assert!(b.else_branch.is_some());
                assert_eq!(b.else_branch.as_ref().map(|v| v.len()), Some(1));
            }
            _ => panic!("expected if block"),
        }
    }

    #[test]
    fn test_for_block() {
        let nodes = parse("@for (item of items; track item.id) { <li>{{ item.name }}</li> }");
        match &nodes[0] {
            TemplateNode::ForBlock(b) => {
                assert_eq!(b.item_name, "item");
                assert_eq!(b.iterable, "items");
                assert_eq!(b.track_expression, "item.id");
                assert_eq!(b.children.len(), 1);
            }
            _ => panic!("expected for block"),
        }
    }

    #[test]
    fn test_switch_block() {
        let nodes =
            parse("@switch (color) { @case ('red') { <p>Red</p> } @default { <p>Other</p> } }");
        match &nodes[0] {
            TemplateNode::SwitchBlock(b) => {
                assert_eq!(b.expression, "color");
                assert_eq!(b.cases.len(), 1);
                assert_eq!(b.cases[0].expression, "'red'");
                assert!(b.default_branch.is_some());
            }
            _ => panic!("expected switch block"),
        }
    }

    #[test]
    fn test_let_block() {
        let nodes = parse("@let _options = options();");
        assert_eq!(nodes.len(), 1);
        match &nodes[0] {
            TemplateNode::LetDeclaration(l) => {
                assert_eq!(l.name, "_options");
                assert_eq!(l.expression, "options()");
            }
            _ => panic!("expected let declaration"),
        }
    }

    #[test]
    fn test_animation_property_binding() {
        let nodes = parse("<div [@fade]=\"state\"></div>");
        match &nodes[0] {
            TemplateNode::Element(e) => match &e.attributes[0] {
                TemplateAttribute::Property { name, expression } => {
                    assert_eq!(name, "@fade");
                    assert_eq!(expression, "state");
                }
                _ => panic!("expected property binding for [@fade]"),
            },
            _ => panic!("expected element"),
        }
    }

    #[test]
    fn test_animation_listener_done() {
        let nodes = parse("<div (@fade.done)=\"onDone($event)\"></div>");
        match &nodes[0] {
            TemplateNode::Element(e) => match &e.attributes[0] {
                TemplateAttribute::Event { name, handler } => {
                    assert_eq!(name, "@fade.done");
                    assert_eq!(handler, "onDone($event)");
                }
                _ => panic!("expected event binding for (@fade.done)"),
            },
            _ => panic!("expected element"),
        }
    }

    #[test]
    fn test_animation_listener_start() {
        let nodes = parse("<div (@fade.start)=\"onStart()\"></div>");
        match &nodes[0] {
            TemplateNode::Element(e) => match &e.attributes[0] {
                TemplateAttribute::Event { name, handler } => {
                    assert_eq!(name, "@fade.start");
                    assert_eq!(handler, "onStart()");
                }
                _ => panic!("expected event binding for (@fade.start)"),
            },
            _ => panic!("expected element"),
        }
    }

    #[test]
    fn test_let_with_if() {
        let nodes = parse("@let x = value(); @if (x) { <p>yes</p> }");
        assert_eq!(nodes.len(), 2);
        assert!(matches!(&nodes[0], TemplateNode::LetDeclaration(_)));
        assert!(matches!(&nodes[1], TemplateNode::IfBlock(_)));
    }
}
