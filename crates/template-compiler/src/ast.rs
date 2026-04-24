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
    /// An `@let` variable declaration.
    LetDeclaration(LetDeclarationNode),
    /// An `@defer` block with optional `@placeholder` / `@loading` / `@error` sub-blocks.
    DeferBlock(DeferBlockNode),
    /// An ICU expression `{ expr, plural|select|selectordinal, ... }`.
    IcuExpression(IcuExpressionNode),
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
    /// A template reference variable like `#myRef` or `#myRef="exportAs"`.
    Reference {
        /// Reference name.
        name: String,
        /// Optional export-as value (e.g. `"ngForm"`).
        export_as: Option<String>,
    },
    /// An `i18n` marker attribute on an element (e.g. `i18n="@@intro|meaning|desc"`).
    /// Indicates this element's text content should be translated.
    I18n(I18nMeta),
    /// An `i18n-<name>` marker attribute for a translatable static attribute
    /// (e.g. `i18n-title="@@tooltip"` alongside `title="Hello"`).
    I18nAttr {
        /// Target attribute name (the `<name>` part of `i18n-<name>`).
        target: String,
        /// Optional translation metadata attached to the marker.
        meta: I18nMeta,
    },
}

/// Parsed `i18n` attribute metadata.
///
/// Angular's `i18n` attribute syntax is `"meaning|description@@id"` — all
/// segments optional. The message text itself comes from the element's
/// content or the target attribute's value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct I18nMeta {
    /// Optional explicit message id (after `@@`).
    pub id: Option<String>,
    /// Optional translator meaning (before `|`).
    pub meaning: Option<String>,
    /// Optional translator description (between `|` and `@@`).
    pub description: Option<String>,
}

impl I18nMeta {
    /// Parse the raw attribute value into an `I18nMeta`. An empty or missing
    /// value returns `I18nMeta::default()` (all fields `None`).
    pub fn parse(raw: Option<&str>) -> Self {
        let s = raw.unwrap_or("").trim();
        if s.is_empty() {
            return Self::default();
        }
        let (head, id) = match s.split_once("@@") {
            Some((h, i)) => (h, Some(i.trim().to_string()).filter(|v| !v.is_empty())),
            None => (s, None),
        };
        let (meaning, description) = match head.split_once('|') {
            Some((m, d)) => {
                let m = m.trim();
                let d = d.trim();
                (
                    (!m.is_empty()).then(|| m.to_string()),
                    (!d.is_empty()).then(|| d.to_string()),
                )
            }
            None => {
                let h = head.trim();
                (None, (!h.is_empty()).then(|| h.to_string()))
            }
        };
        Self {
            id,
            meaning,
            description,
        }
    }
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

/// An `@let` variable declaration: `@let name = expression;`
#[derive(Debug, Clone, PartialEq)]
pub struct LetDeclarationNode {
    /// The variable name (e.g. `_options`).
    pub name: String,
    /// The initializer expression (e.g. `options()`).
    pub expression: String,
}

/// A `@case` branch.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseBranch {
    /// The case expression.
    pub expression: String,
    /// Children rendered when matched.
    pub children: Vec<TemplateNode>,
}

/// An `@defer` block.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferBlockNode {
    /// Triggers that fetch and render the deferred content.
    pub triggers: Vec<DeferTrigger>,
    /// Triggers with the `prefetch` prefix — fetch without rendering.
    pub prefetch_triggers: Vec<DeferTrigger>,
    /// Main deferred content.
    pub children: Vec<TemplateNode>,
    /// Optional `@placeholder { ... }` block (rendered before trigger fires).
    pub placeholder: Option<Vec<TemplateNode>>,
    /// Optional `@loading { ... }` block (rendered while deferred resources load).
    pub loading: Option<Vec<TemplateNode>>,
    /// Optional `@error { ... }` block (rendered if loading fails).
    pub error: Option<Vec<TemplateNode>>,
}

/// An ICU message node: `{ expr, plural|select|selectordinal, case {body} ... }`.
#[derive(Debug, Clone, PartialEq)]
pub struct IcuExpressionNode {
    /// The switch expression (e.g. `count`, `gender`).
    pub switch_expression: String,
    /// Message category (`plural`, `select`, or `selectordinal`).
    pub category: IcuCategory,
    /// Case branches (`=0`, `one`, `other`, ...).
    pub cases: Vec<IcuCase>,
}

/// ICU message category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IcuCategory {
    /// Cardinal pluralization (`plural`).
    Plural,
    /// Ordinal pluralization (`selectordinal`).
    SelectOrdinal,
    /// Keyed selection (`select`).
    Select,
}

/// A single ICU case branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcuCase {
    /// The case key (`=0`, `one`, `other`, `male`, ...).
    pub key: String,
    /// The raw case body content (may contain `#` / `{{...}}` placeholders).
    pub body: String,
}

/// A single `@defer` trigger. `viewport` / `hover` / `interaction` may carry
/// an optional template-reference name (e.g. `on hover(triggerRef)`); ngc-rs
/// records the reference for future wiring but currently emits the keyword-
/// only form of the runtime instruction.
#[derive(Debug, Clone, PartialEq)]
pub enum DeferTrigger {
    /// `on viewport` / `on viewport(ref)`.
    Viewport(Option<String>),
    /// `on idle`.
    Idle,
    /// `on immediate`.
    Immediate,
    /// `on hover` / `on hover(ref)`.
    Hover(Option<String>),
    /// `on interaction` / `on interaction(ref)`.
    Interaction(Option<String>),
    /// `on timer(<duration>)` — duration stored verbatim (e.g. `500ms`).
    Timer(String),
    /// `when <expression>` — expression evaluated each change detection cycle.
    When(String),
}
