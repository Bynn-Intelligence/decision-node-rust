//! Question builders: [`truth`], [`choice`], [`score`], [`number`], [`point`]
//! and [`bounding_box`].
//!
//! Each builder produces the wire form of one question. Instructions, option
//! descriptions and levels take a string or any value that serialises to JSON,
//! as the API does. A plain [`serde_json::Value`] in the wire form is accepted
//! wherever a builder is, so a request copied from the documentation runs
//! unchanged.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::{Map, Value};

fn to_json<T: Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| format!("a value cannot be serialised to JSON: {e}"))
}

/// One question, in its wire form. Built from [`Truth`], [`Choice`], [`Score`],
/// [`Number`], [`Point`], [`BoundingBox`] or a [`serde_json::Value`].
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    inner: Result<Value, String>,
}

impl Question {
    /// The wire form of the question, or why it could not be built.
    pub fn to_value(&self) -> Result<&Value, &str> {
        self.inner.as_ref().map_err(String::as_str)
    }

    pub(crate) fn into_value(self) -> Result<Value, String> {
        self.inner
    }
}

impl From<Value> for Question {
    fn from(value: Value) -> Question {
        Question { inner: Ok(value) }
    }
}

impl From<Truth> for Question {
    fn from(t: Truth) -> Question {
        Question { inner: t.build() }
    }
}

impl From<Choice> for Question {
    fn from(c: Choice) -> Question {
        Question { inner: c.build() }
    }
}

impl From<Score> for Question {
    fn from(s: Score) -> Question {
        Question { inner: s.build() }
    }
}

impl From<Number> for Question {
    fn from(n: Number) -> Question {
        Question { inner: n.build() }
    }
}

impl From<Point> for Question {
    fn from(p: Point) -> Question {
        Question { inner: p.0.build("point") }
    }
}

impl From<BoundingBox> for Question {
    fn from(b: BoundingBox) -> Question {
        Question { inner: b.0.build("box") }
    }
}

// --------------------------------------------------------------------------- truth

/// A `truth` question: is this statement true? Answered by one probability.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Truth {
    instructions: Option<Result<Value, String>>,
    if_true: Option<Result<Value, String>>,
    if_false: Option<Result<Value, String>>,
}

/// A `truth` question with these instructions.
///
/// ```
/// use decisionnode::truth;
///
/// let q = truth("Escalate to a human now?")
///     .if_true("the customer is angry or the issue is urgent")
///     .if_false("a routine request");
/// ```
pub fn truth(instructions: impl Serialize) -> Truth {
    Truth::new().instructions(instructions)
}

impl Truth {
    /// A `truth` question without instructions. It needs at least one side of
    /// the criteria.
    pub fn new() -> Truth {
        Truth::default()
    }

    /// What to decide: a string, or any value that serialises to JSON.
    pub fn instructions(mut self, instructions: impl Serialize) -> Truth {
        self.instructions = Some(to_json(instructions));
        self
    }

    /// What a yes means. Sent as `criteria.true`.
    pub fn if_true(mut self, description: impl Serialize) -> Truth {
        self.if_true = Some(to_json(description));
        self
    }

    /// What a no means. Sent as `criteria.false`.
    pub fn if_false(mut self, description: impl Serialize) -> Truth {
        self.if_false = Some(to_json(description));
        self
    }

    fn build(self) -> Result<Value, String> {
        let mut q = Map::new();
        q.insert("type".into(), "truth".into());
        if let Some(i) = self.instructions {
            q.insert("instructions".into(), i?);
        }
        let mut criteria = Map::new();
        if let Some(t) = self.if_true {
            criteria.insert("true".into(), t?);
        }
        if let Some(f) = self.if_false {
            criteria.insert("false".into(), f?);
        }
        if !criteria.is_empty() {
            q.insert("criteria".into(), Value::Object(criteria));
        }
        Ok(Value::Object(q))
    }
}

impl From<&str> for Truth {
    fn from(instructions: &str) -> Truth {
        truth(instructions)
    }
}

impl From<String> for Truth {
    fn from(instructions: String) -> Truth {
        truth(instructions)
    }
}

impl From<&String> for Truth {
    fn from(instructions: &String) -> Truth {
        truth(instructions)
    }
}

// --------------------------------------------------------------------------- choice

/// The options of a [`Choice`]: a map of name to description, or a list of
/// names (each description `null`).
///
/// Option names are sent byte for byte and in the order given.
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    inner: Result<Map<String, Value>, String>,
}

impl Options {
    fn names<I, S>(names: I) -> Options
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Options { inner: Ok(names.into_iter().map(|n| (n.into(), Value::Null)).collect()) }
    }

    fn pairs<I, K, D>(pairs: I) -> Options
    where
        I: IntoIterator<Item = (K, D)>,
        K: Into<String>,
        D: Serialize,
    {
        let inner = pairs.into_iter().map(|(k, d)| Ok((k.into(), to_json(d)?))).collect();
        Options { inner }
    }
}

impl<const N: usize> From<[&str; N]> for Options {
    fn from(names: [&str; N]) -> Options {
        Options::names(names)
    }
}

impl From<&[&str]> for Options {
    fn from(names: &[&str]) -> Options {
        Options::names(names.iter().copied())
    }
}

impl From<Vec<&str>> for Options {
    fn from(names: Vec<&str>) -> Options {
        Options::names(names)
    }
}

impl From<Vec<String>> for Options {
    fn from(names: Vec<String>) -> Options {
        Options::names(names)
    }
}

impl<K: Into<String>, D: Serialize, const N: usize> From<[(K, D); N]> for Options {
    fn from(pairs: [(K, D); N]) -> Options {
        Options::pairs(pairs)
    }
}

impl<K: Into<String>, D: Serialize> From<Vec<(K, D)>> for Options {
    fn from(pairs: Vec<(K, D)>) -> Options {
        Options::pairs(pairs)
    }
}

impl<K: Into<String>, D: Serialize> From<BTreeMap<K, D>> for Options {
    fn from(map: BTreeMap<K, D>) -> Options {
        Options::pairs(map)
    }
}

impl<K: Into<String>, D: Serialize, S> From<HashMap<K, D, S>> for Options {
    fn from(map: HashMap<K, D, S>) -> Options {
        Options::pairs(map)
    }
}

impl<K: Into<String>, D: Serialize> From<indexmap::IndexMap<K, D>> for Options {
    fn from(map: indexmap::IndexMap<K, D>) -> Options {
        Options::pairs(map)
    }
}

impl From<Map<String, Value>> for Options {
    fn from(map: Map<String, Value>) -> Options {
        Options { inner: Ok(map) }
    }
}

/// A JSON object of name to description, or a JSON array of names.
impl From<Value> for Options {
    fn from(value: Value) -> Options {
        match value {
            Value::Object(map) => Options { inner: Ok(map) },
            Value::Array(items) => {
                let inner = items
                    .into_iter()
                    .map(|v| match v {
                        Value::String(s) => Ok((s, Value::Null)),
                        other => Err(format!("an option name must be a string, got {other}")),
                    })
                    .collect();
                Options { inner }
            }
            other => Options { inner: Err(format!("options must be an object or a list of names, got {other}")) },
        }
    }
}

/// A `choice` question: pick exactly one of the options.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    instructions: Option<Result<Value, String>>,
    options: Options,
}

/// A `choice` question with these instructions and options.
///
/// ```
/// use decisionnode::choice;
///
/// let by_name = choice("Where should this go?", ["billing", "bug", "account"]);
/// let described = choice("Where should this go?", [("billing", "money"), ("bug", "broken")]);
/// ```
pub fn choice(instructions: impl Serialize, options: impl Into<Options>) -> Choice {
    Choice::new(options).instructions(instructions)
}

impl Choice {
    /// A `choice` question without instructions, answered from its options.
    pub fn new(options: impl Into<Options>) -> Choice {
        Choice { instructions: None, options: options.into() }
    }

    /// What to decide: a string, or any value that serialises to JSON.
    pub fn instructions(mut self, instructions: impl Serialize) -> Choice {
        self.instructions = Some(to_json(instructions));
        self
    }

    fn build(self) -> Result<Value, String> {
        let mut q = Map::new();
        q.insert("type".into(), "choice".into());
        if let Some(i) = self.instructions {
            q.insert("instructions".into(), i?);
        }
        q.insert("criteria".into(), Value::Object(self.options.inner?));
        Ok(Value::Object(q))
    }
}

// --------------------------------------------------------------------------- score

/// The levels of a [`Score`], ordered low to high. Each level is a string or
/// any value that serialises to JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct Levels {
    inner: Result<Vec<Value>, String>,
}

impl Levels {
    fn items<I, T>(items: I) -> Levels
    where
        I: IntoIterator<Item = T>,
        T: Serialize,
    {
        Levels { inner: items.into_iter().map(to_json).collect() }
    }
}

impl<T: Serialize, const N: usize> From<[T; N]> for Levels {
    fn from(levels: [T; N]) -> Levels {
        Levels::items(levels)
    }
}

impl<T: Serialize> From<Vec<T>> for Levels {
    fn from(levels: Vec<T>) -> Levels {
        Levels::items(levels)
    }
}

impl<T: Serialize> From<&[T]> for Levels {
    fn from(levels: &[T]) -> Levels {
        Levels::items(levels)
    }
}

/// A JSON array of levels.
impl From<Value> for Levels {
    fn from(value: Value) -> Levels {
        match value {
            Value::Array(items) => Levels { inner: Ok(items) },
            other => Levels { inner: Err(format!("levels must be a list, got {other}")) },
        }
    }
}

/// A `score` question: place the state on an ordered scale.
#[derive(Clone, Debug, PartialEq)]
pub struct Score {
    instructions: Option<Result<Value, String>>,
    levels: Levels,
}

/// A `score` question with these instructions and levels, low to high.
///
/// ```
/// use decisionnode::score;
///
/// let q = score("How urgent is this?", ["routine", "today", "urgent", "critical"]);
/// ```
pub fn score(instructions: impl Serialize, levels: impl Into<Levels>) -> Score {
    Score::new(levels).instructions(instructions)
}

impl Score {
    /// A `score` question without instructions, answered from its levels.
    pub fn new(levels: impl Into<Levels>) -> Score {
        Score { instructions: None, levels: levels.into() }
    }

    /// What to decide: a string, or any value that serialises to JSON.
    pub fn instructions(mut self, instructions: impl Serialize) -> Score {
        self.instructions = Some(to_json(instructions));
        self
    }

    fn build(self) -> Result<Value, String> {
        let mut q = Map::new();
        q.insert("type".into(), "score".into());
        if let Some(i) = self.instructions {
            q.insert("instructions".into(), i?);
        }
        q.insert("criteria".into(), Value::Array(self.levels.inner?));
        Ok(Value::Object(q))
    }
}

// --------------------------------------------------------------------------- number

/// A `number` question: read or estimate a value on a grid from `min` to
/// `max` in steps of `step`. Answered by the most likely value, the expected
/// value and the probability of each likely value.
///
/// `min`, `max` and `step` are left out of the wire form unless given, so the
/// API's defaults apply: 0, 255 and 1. A grid holds at most 4,096 values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Number {
    instructions: Option<Result<Value, String>>,
    min: Option<Result<Value, String>>,
    max: Option<Result<Value, String>>,
    step: Option<Result<Value, String>>,
}

/// A `number` question with these instructions.
///
/// ```
/// use decisionnode::number;
///
/// let cars = number("How many cars are visible in the image?").min(0).max(50);
/// let rating = number("Rate the review from 0 to 5.").max(5).step(0.5);
/// ```
pub fn number(instructions: impl Serialize) -> Number {
    Number::new().instructions(instructions)
}

impl Number {
    /// A `number` question without instructions. The API needs them, so add
    /// them with [`Number::instructions`].
    pub fn new() -> Number {
        Number::default()
    }

    /// What to count or read: a string, or any value that serialises to JSON.
    pub fn instructions(mut self, instructions: impl Serialize) -> Number {
        self.instructions = Some(to_json(instructions));
        self
    }

    /// The lowest value. Default 0.
    pub fn min(mut self, min: impl Serialize) -> Number {
        self.min = Some(to_json(min));
        self
    }

    /// The highest value the grid may reach; it need not be on the grid.
    /// Default 255.
    pub fn max(mut self, max: impl Serialize) -> Number {
        self.max = Some(to_json(max));
        self
    }

    /// The spacing of the grid, above 0. Default 1.
    pub fn step(mut self, step: impl Serialize) -> Number {
        self.step = Some(to_json(step));
        self
    }

    fn build(self) -> Result<Value, String> {
        let mut q = Map::new();
        q.insert("type".into(), "number".into());
        for (name, value) in
            [("instructions", self.instructions), ("min", self.min), ("max", self.max), ("step", self.step)]
        {
            if let Some(v) = value {
                q.insert(name.into(), v?);
            }
        }
        Ok(Value::Object(q))
    }
}

// --------------------------------------------------------------------------- point and box

/// What a `point` and a `box` question share: the instructions, the image to
/// look in and the units.
#[derive(Clone, Debug, Default, PartialEq)]
struct Place {
    instructions: Option<Result<Value, String>>,
    image: Option<String>,
    units: Option<String>,
}

impl Place {
    fn build(self, qtype: &str) -> Result<Value, String> {
        let mut q = Map::new();
        q.insert("type".into(), qtype.into());
        if let Some(i) = self.instructions {
            q.insert("instructions".into(), i?);
        }
        for (name, value) in [("image", self.image), ("units", self.units)] {
            if let Some(v) = value {
                q.insert(name.into(), Value::String(v));
            }
        }
        Ok(Value::Object(q))
    }
}

/// A `point` question: one spot in an image, answered by a
/// [`crate::PointAnswer`]: `x` and `y`, the probability that what the
/// instructions describe is there at all (`present`) and that the spot lies
/// on it (`confidence`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Point(Place);

/// A `point` question with these instructions. The request needs an image;
/// with two or more, name the one to look in with [`Point::image`].
///
/// ```
/// use decisionnode::point;
///
/// let pick = point("Point to the yellow package closest to the robot arm.");
/// let exact = point("Where is the crack?").image("belt").units("normalized");
/// ```
pub fn point(instructions: impl Serialize) -> Point {
    Point::new().instructions(instructions)
}

impl Point {
    /// A `point` question without instructions. The API needs them, so add
    /// them with [`Point::instructions`].
    pub fn new() -> Point {
        Point::default()
    }

    /// What to find: a string, or any value that serialises to JSON.
    pub fn instructions(mut self, instructions: impl Serialize) -> Point {
        self.0.instructions = Some(to_json(instructions));
        self
    }

    /// The id of the image to look in (a string), required when the request
    /// has two or more images. A reference by number ("Picture 2") goes in
    /// the instructions.
    pub fn image(mut self, id: impl Into<String>) -> Point {
        self.0.image = Some(id.into());
        self
    }

    /// `"pixels"` (the default: pixels of the image as sent) or
    /// `"normalized"` (fractions of the width and the height).
    pub fn units(mut self, units: impl Into<String>) -> Point {
        self.0.units = Some(units.into());
        self
    }
}

/// A `box` question: a rectangle in an image, answered by a
/// [`crate::BoxAnswer`]: its top-left (`x1`, `y1`) and bottom-right (`x2`,
/// `y2`) corners, with `present` and `confidence` as a point has. Named for
/// the wire type `box`, a reserved word in Rust.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BoundingBox(Place);

/// A `box` question with these instructions. The request needs an image;
/// with two or more, name the one to look in with [`BoundingBox::image`].
///
/// ```
/// use decisionnode::bounding_box;
///
/// let damage = bounding_box("Draw a box around the damaged package.").image("belt");
/// ```
pub fn bounding_box(instructions: impl Serialize) -> BoundingBox {
    BoundingBox::new().instructions(instructions)
}

impl BoundingBox {
    /// A `box` question without instructions. The API needs them, so add
    /// them with [`BoundingBox::instructions`].
    pub fn new() -> BoundingBox {
        BoundingBox::default()
    }

    /// What to find: a string, or any value that serialises to JSON.
    pub fn instructions(mut self, instructions: impl Serialize) -> BoundingBox {
        self.0.instructions = Some(to_json(instructions));
        self
    }

    /// The id of the image to look in (a string), required when the request
    /// has two or more images. A reference by number ("Picture 2") goes in
    /// the instructions.
    pub fn image(mut self, id: impl Into<String>) -> BoundingBox {
        self.0.image = Some(id.into());
        self
    }

    /// `"pixels"` (the default) or `"normalized"`.
    pub fn units(mut self, units: impl Into<String>) -> BoundingBox {
        self.0.units = Some(units.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn point_and_box_wire_forms() {
        assert_eq!(wire(point("Where?")), json!({"type": "point", "instructions": "Where?"}));
        assert_eq!(
            wire(bounding_box("Where?").image("belt").units("normalized")),
            json!({"type": "box", "instructions": "Where?", "image": "belt", "units": "normalized"})
        );
        assert_eq!(wire(Point::new().image("a")), json!({"type": "point", "image": "a"}));
    }

    fn wire(q: impl Into<Question>) -> Value {
        q.into().into_value().unwrap()
    }

    #[test]
    fn truth_without_criteria() {
        assert_eq!(wire(truth("Escalate?")), json!({"type": "truth", "instructions": "Escalate?"}));
    }

    #[test]
    fn truth_with_one_side() {
        assert_eq!(
            wire(truth("Spam?").if_true("bulk advertising")),
            json!({"type": "truth", "instructions": "Spam?", "criteria": {"true": "bulk advertising"}})
        );
    }

    #[test]
    fn truth_criteria_only() {
        assert_eq!(
            wire(Truth::new().if_true("yes").if_false("no")),
            json!({"type": "truth", "criteria": {"true": "yes", "false": "no"}})
        );
    }

    #[test]
    fn choice_from_names_keeps_order() {
        let v = wire(choice("Where?", ["zeta", "alpha", ""]));
        assert_eq!(
            v,
            json!({"type": "choice", "instructions": "Where?", "criteria": {"zeta": null, "alpha": null, "": null}})
        );
        let keys: Vec<_> = v["criteria"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["zeta", "alpha", ""]);
    }

    #[test]
    fn choice_from_pairs_with_structured_descriptions() {
        let v = wire(choice("Where?", vec![("billing", json!("money")), ("bug", json!({"what": "broken"}))]));
        assert_eq!(v["criteria"], json!({"billing": "money", "bug": {"what": "broken"}}));
    }

    #[test]
    fn choice_from_json() {
        assert_eq!(wire(Choice::new(json!(["a", "b"])))["criteria"], json!({"a": null, "b": null}));
        assert!(Question::from(Choice::new(json!(5))).into_value().is_err());
    }

    #[test]
    fn score_with_structured_levels() {
        let v = wire(score(
            json!({"question": "How risky?"}),
            vec![json!({"what": "none"}), json!(["high", "risk"]), json!("critical")],
        ));
        assert_eq!(
            v,
            json!({"type": "score", "instructions": {"question": "How risky?"}, "criteria": [{"what": "none"}, ["high", "risk"], "critical"]})
        );
    }

    #[test]
    fn wire_maps_pass_through() {
        let raw = json!({"type": "truth", "instructions": "?", "criteria": {"true": "yes", "false": null}});
        assert_eq!(wire(raw.clone()), raw);
    }

    #[test]
    fn number_leaves_out_what_is_not_given() {
        assert_eq!(wire(number("How many?")), json!({"type": "number", "instructions": "How many?"}));
        assert_eq!(
            wire(number("Rating?").min(-2.5).max(5).step(0.25)),
            json!({"type": "number", "instructions": "Rating?", "min": -2.5, "max": 5, "step": 0.25})
        );
        assert_eq!(wire(Number::new().min(0).max(10)), json!({"type": "number", "min": 0, "max": 10}));
    }

    #[test]
    fn unserialisable_values_are_reported() {
        let mut bad = HashMap::new();
        bad.insert(vec![1u8], 1);
        assert!(Question::from(truth(bad)).into_value().is_err());
    }
}
