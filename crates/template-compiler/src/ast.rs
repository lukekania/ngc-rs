/// A node in the parsed template AST.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplateNode {
    /// An HTML element (paired or void/self-closing).
    Element(ElementNode),
    /// A text node.
    Text(TextNode),
    /// An interpolation expression `{{ expr }}`.
    Interpolation(InterpolationNode),
    /// An `@if` / `@else if` / `@else` control flow block.
    IfBlock(IfBlockNode),
    /// An `@for` / `@empty` control flow block.
    ForBlock(ForBlockNode),
    /// An `@switch` / `@case` / `@default` control flow block.
    SwitchBlock(SwitchBlockNode),
}

/// An HTML element node.
#[derive(Debug, Clone, PartialEq)]
pub struct ElementNode {
    /// The tag name (e.g. `div`, `router-outlet`).
    pub tag: String,
    /// The element's attributes.
    pub attributes: Vec<TemplateAttribute>,
    /// Child nodes (empty for void/self-closing elements).
    pub children: Vec<TemplateNode>,
    /// Whether this is a self-closing/void element.
    pub is_void: bool,
}

/// An attribute on an element.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplateAttribute {
    /// A static attribute like `class="foo"`.
    Static {
        /// Attribute name.
        name: String,
        /// Attribute value (None for boolean attributes like `disabled`).
        value: Option<String>,
    },
    /// A property binding like `[title]="expr"`.
    Property {
        /// Property name.
        name: String,
        /// JavaScript expression.
        expression: String,
    },
    /// A class binding like `[class.active]="expr"`.
    ClassBinding {
        /// CSS class name.
        class_name: String,
        /// JavaScript expression.
        expression: String,
    },
    /// A style binding like `[style.color]="expr"`.
    StyleBinding {
        /// CSS property name.
        property: String,
        /// JavaScript expression.
        expression: String,
    },
    /// An attribute binding like `[attr.aria-label]="expr"`.
    AttrBinding {
        /// Attribute name.
        name: String,
        /// JavaScript expression.
        expression: String,
    },
    /// An event binding like `(click)="handler()"`.
    Event {
        /// Event name.
        name: String,
        /// Handler expression.
        handler: String,
    },
    /// A two-way binding like `[(ngModel)]="expr"`.
    TwoWayBinding {
        /// Property name.
        name: String,
        /// JavaScript expression.
        expression: String,
    },
    /// A structural directive like `*ngIf="condition"`.
    StructuralDirective {
        /// Directive name (e.g. `ngIf`, `ngFor`).
        name: String,
        /// Directive expression.
        expression: String,
    },
    /// A template reference variable like `#myRef`.
    Reference {
        /// Reference name.
        name: String,
    },
}

/// A text node.
#[derive(Debug, Clone, PartialEq)]
pub struct TextNode {
    /// The text content.
    pub value: String,
}

/// An interpolation node `{{ expression }}`.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpolationNode {
    /// The raw JavaScript expression.
    pub expression: String,
    /// Parsed pipe chain, if any.
    pub pipes: Vec<PipeCall>,
}

/// A pipe call in an interpolation expression.
#[derive(Debug, Clone, PartialEq)]
pub struct PipeCall {
    /// Pipe name.
    pub name: String,
    /// Pipe arguments.
    pub args: Vec<String>,
}

/// An `@if` block.
#[derive(Debug, Clone, PartialEq)]
pub struct IfBlockNode {
    /// The condition expression.
    pub condition: String,
    /// Children rendered when condition is true.
    pub children: Vec<TemplateNode>,
    /// Optional `@else if` branches.
    pub else_if_branches: Vec<ElseIfBranch>,
    /// Optional `@else` branch.
    pub else_branch: Option<Vec<TemplateNode>>,
}

/// An `@else if` branch.
#[derive(Debug, Clone, PartialEq)]
pub struct ElseIfBranch {
    /// The condition expression.
    pub condition: String,
    /// Children rendered when condition is true.
    pub children: Vec<TemplateNode>,
}

/// An `@for` block.
#[derive(Debug, Clone, PartialEq)]
pub struct ForBlockNode {
    /// The loop variable name.
    pub item_name: String,
    /// The iterable expression.
    pub iterable: String,
    /// The track expression.
    pub track_expression: String,
    /// Children rendered for each item.
    pub children: Vec<TemplateNode>,
    /// Optional `@empty` children.
    pub empty_children: Option<Vec<TemplateNode>>,
}

/// An `@switch` block.
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchBlockNode {
    /// The switch expression.
    pub expression: String,
    /// Case branches.
    pub cases: Vec<CaseBranch>,
    /// Optional default branch.
    pub default_branch: Option<Vec<TemplateNode>>,
}

/// A `@case` branch.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseBranch {
    /// The case expression.
    pub expression: String,
    /// Children rendered when matched.
    pub children: Vec<TemplateNode>,
}
