//! Unevaluated strings with `$var` references (ninja's `EvalString`).

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalPart {
    Literal(String),
    Var(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvalString {
    parts: Vec<EvalPart>,
}

impl EvalString {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn literal(s: &str) -> Self {
        let mut e = Self::new();
        e.add_text(s);
        e
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    pub fn clear(&mut self) {
        self.parts.clear();
    }

    pub fn add_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(EvalPart::Literal(last)) = self.parts.last_mut() {
            last.push_str(text);
        } else {
            self.parts.push(EvalPart::Literal(text.to_string()));
        }
    }

    pub fn add_var(&mut self, name: &str) {
        self.parts.push(EvalPart::Var(name.to_string()));
    }

    pub fn parts(&self) -> &[EvalPart] {
        &self.parts
    }

    /// Evaluates with `lookup` resolving variable references.
    pub fn evaluate_with<E>(&self, lookup: &mut dyn FnMut(&str) -> Result<String, E>) -> Result<String, E> {
        // Most paths are a single literal.
        if let [EvalPart::Literal(s)] = self.parts.as_slice() {
            return Ok(s.clone());
        }
        let mut out = String::new();
        for p in &self.parts {
            match p {
                EvalPart::Literal(s) => out.push_str(s),
                EvalPart::Var(v) => out.push_str(&lookup(v)?),
            }
        }
        Ok(out)
    }

    /// Like [`Self::evaluate`], but moves a single literal out without copying.
    pub fn into_evaluated(mut self, lookup: &mut dyn FnMut(&str) -> String) -> String {
        if let [EvalPart::Literal(_)] = self.parts.as_slice()
            && let Some(EvalPart::Literal(s)) = self.parts.pop()
        {
            return s;
        }
        self.evaluate(lookup)
    }

    /// Evaluates with an infallible lookup.
    pub fn evaluate(&self, lookup: &mut dyn FnMut(&str) -> String) -> String {
        let r: Result<String, std::convert::Infallible> = self.evaluate_with(&mut |v| Ok(lookup(v)));
        match r {
            Ok(s) => s,
        }
    }

    /// Debug form used by tests, as ninja's `Serialize`: `[text][$var]`.
    pub fn serialize(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                EvalPart::Literal(s) => {
                    out.push('[');
                    out.push_str(s);
                    out.push(']');
                }
                EvalPart::Var(v) => {
                    out.push_str("[$");
                    out.push_str(v);
                    out.push(']');
                }
            }
        }
        out
    }

    /// Source-like form: `text${var}`.
    pub fn unparse(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                EvalPart::Literal(s) => out.push_str(s),
                EvalPart::Var(v) => {
                    out.push_str("${");
                    out.push_str(v);
                    out.push('}');
                }
            }
        }
        out
    }
}
