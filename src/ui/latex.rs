// Terminal-native math layout. It keeps fractions and radicals two-dimensional
// without depending on a TeX installation or terminal image support.
use super::display_width;

pub(super) fn pretty(source: &str) -> String {
    let source = source.trim().trim_matches('$');
    typeset(source, 0).lines.join("\n")
}

struct MathBox {
    lines: Vec<String>,
    baseline: usize,
    width: usize,
}

impl MathBox {
    fn leaf(source: String) -> Self {
        let width = display_width(&source);
        Self {
            lines: vec![source],
            baseline: 0,
            width,
        }
    }
    fn beside(self, other: Self) -> Self {
        let baseline = self.baseline.max(other.baseline);
        let bottom =
            (self.lines.len() - self.baseline - 1).max(other.lines.len() - other.baseline - 1);
        let height = baseline + bottom + 1;
        let mut lines = Vec::with_capacity(height);
        for row in 0..height {
            let left = row
                .checked_sub(baseline - self.baseline)
                .and_then(|index| self.lines.get(index))
                .cloned()
                .unwrap_or_else(|| " ".repeat(self.width));
            let right = row
                .checked_sub(baseline - other.baseline)
                .and_then(|index| other.lines.get(index))
                .cloned()
                .unwrap_or_else(|| " ".repeat(other.width));
            lines.push(format!("{left}{right}"));
        }
        Self {
            lines,
            baseline,
            width: self.width + other.width,
        }
    }
    fn fraction(top: Self, bottom: Self) -> Self {
        let width = top.width.max(bottom.width).max(1) + 2;
        let mut lines = Vec::new();
        for line in top.lines {
            lines.push(center(&line, width));
        }
        let baseline = lines.len();
        lines.push("─".repeat(width));
        for line in bottom.lines {
            lines.push(center(&line, width));
        }
        Self {
            lines,
            baseline,
            width,
        }
    }
    fn root(body: Self) -> Self {
        let width = body.width + 2;
        let mut lines = vec![format!("  {}", "─".repeat(body.width))];
        for (index, line) in body.lines.into_iter().enumerate() {
            lines.push(format!(
                "{} {line}",
                if index == body.baseline { '√' } else { ' ' }
            ));
        }
        Self {
            lines,
            baseline: body.baseline + 1,
            width,
        }
    }
    fn scripts(upper: Option<Self>, lower: Option<Self>) -> Self {
        let width = upper
            .as_ref()
            .map_or(0, |part| part.width)
            .max(lower.as_ref().map_or(0, |part| part.width))
            .max(1);
        let mut lines = Vec::new();
        if let Some(upper) = upper {
            lines.extend(upper.lines.into_iter().map(|line| center(&line, width)));
        }
        let baseline = lines.len();
        lines.push(" ".repeat(width));
        if let Some(lower) = lower {
            lines.extend(lower.lines.into_iter().map(|line| center(&line, width)));
        }
        Self {
            lines,
            baseline,
            width,
        }
    }
    fn line_above(body: Self) -> Self {
        let mut lines = Vec::with_capacity(body.lines.len() + 1);
        lines.push("─".repeat(body.width.max(1)));
        lines.extend(body.lines);
        Self {
            lines,
            baseline: body.baseline + 1,
            width: body.width.max(1),
        }
    }
    fn line_below(mut body: Self) -> Self {
        body.lines.push("─".repeat(body.width.max(1)));
        body
    }
    fn matrix(source: &str, left: &str, right: &str, depth: usize) -> Self {
        let rows: Vec<Vec<Self>> = source
            .split(r"\\")
            .map(|row| {
                row.split('&')
                    .map(|cell| typeset(cell.trim(), depth + 1))
                    .collect()
            })
            .collect();
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let widths: Vec<usize> = (0..columns)
            .map(|column| {
                rows.iter()
                    .filter_map(|row| row.get(column))
                    .map(|part| part.width)
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let mut lines = Vec::new();
        for row in rows {
            let height = row.iter().map(|part| part.lines.len()).max().unwrap_or(1);
            for line in 0..height {
                let body = widths
                    .iter()
                    .enumerate()
                    .map(|(column, width)| {
                        row.get(column)
                            .and_then(|part| part.lines.get(line))
                            .map_or_else(|| " ".repeat(*width), |value| center(value, *width))
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                lines.push(format!("{left}{body}{right}"));
            }
        }
        let width = lines
            .iter()
            .map(|line| display_width(line))
            .max()
            .unwrap_or(1);
        let baseline = lines.len().saturating_sub(1) / 2;
        Self {
            lines,
            baseline,
            width,
        }
    }
}

fn center(text: &str, width: usize) -> String {
    let used = display_width(text);
    let left = width.saturating_sub(used) / 2;
    format!(
        "{}{}{}",
        " ".repeat(left),
        text,
        " ".repeat(width.saturating_sub(used + left))
    )
}

fn braced(source: &str, at: usize) -> Option<(&str, usize)> {
    if source.as_bytes().get(at) != Some(&b'{') {
        return None;
    }
    let mut depth = 0;
    for (offset, ch) in source[at..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&source[at + 1..at + offset], at + offset + 1));
                }
            }
            _ => {}
        }
    }
    None
}

fn argument(source: &str, at: usize) -> Option<(&str, usize)> {
    let at = source[at..]
        .find(|ch: char| !ch.is_whitespace())
        .map(|skip| at + skip)?;
    if source.as_bytes().get(at) == Some(&b'{') {
        braced(source, at)
    } else if source.as_bytes().get(at) == Some(&b'\\') {
        let bytes = source.as_bytes();
        let mut end = at + 1;
        while bytes.get(end).is_some_and(u8::is_ascii_alphabetic) {
            end += 1;
        }
        Some((
            &source[at..end.max(at + 2).min(source.len())],
            end.max(at + 2).min(source.len()),
        ))
    } else {
        let end = at + source[at..].chars().next()?.len_utf8();
        Some((&source[at..end], end))
    }
}

#[cfg(test)]
mod layout_tests {
    use super::pretty;

    #[test]
    fn scripts_and_indexed_roots_span_multiple_terminal_rows() {
        let result = pretty(r"x_{i=0}^{n+1}+\sqrt[3]{y}");
        assert!(result.lines().count() >= 3, "{result}");
        assert!(result.contains("i=0"), "{result}");
        assert!(result.contains("n+1"), "{result}");
        assert!(result.contains('√'), "{result}");
    }

    #[test]
    fn matrix_and_accents_keep_columns_and_lines() {
        let result = pretty(r"\begin{pmatrix}a&\frac{1}{2}\\c&d\end{pmatrix}");
        assert!(result.lines().count() >= 3, "{result}");
        assert!(result.contains("(a"), "{result}");
        assert!(result.contains("c"), "{result}");
        assert!(pretty(r"\overline{x}").starts_with('─'));
        assert!(pretty(r"\underline{x}").ends_with('─'));
    }
}

fn square_argument(source: &str, at: usize) -> Option<(&str, usize)> {
    let tail = source.get(at..)?.strip_prefix('[')?;
    let end = tail.find(']')?;
    Some((&tail[..end], at + end + 2))
}

fn flush_plain(result: &mut MathBox, source: &str, start: usize, end: usize) {
    if start < end {
        let previous = std::mem::replace(result, MathBox::leaf(String::new()));
        *result = previous.beside(MathBox::leaf(text(&source[start..end])));
    }
}

fn typeset(source: &str, depth: usize) -> MathBox {
    if depth >= 8 || source.len() > 4096 {
        return MathBox::leaf(text(source));
    }
    let mut result = MathBox::leaf(String::new());
    let mut plain = 0;
    let mut at = 0;
    while at < source.len() {
        let command = [
            r"\frac",
            r"\dfrac",
            r"\tfrac",
            r"\sqrt",
            r"\binom",
            r"\overline",
            r"\underline",
            r"\boxed",
        ]
        .into_iter()
        .find(|command| source[at..].starts_with(command));
        if let Some(command) = command {
            let after = at + command.len();
            let parsed = if command == r"\sqrt" {
                let (degree, body_at) = square_argument(source, after)
                    .map_or((None, after), |(degree, end)| (Some(degree), end));
                braced(source, body_at).map(|(body, end)| {
                    let root = MathBox::root(typeset(body, depth + 1));
                    let root = if let Some(degree) = degree {
                        MathBox::leaf(text(degree)).beside(root)
                    } else {
                        root
                    };
                    (root, end)
                })
            } else if matches!(command, r"\overline" | r"\underline" | r"\boxed") {
                braced(source, after).map(|(body, end)| {
                    let part = typeset(body, depth + 1);
                    let part = match command {
                        r"\overline" => MathBox::line_above(part),
                        r"\underline" => MathBox::line_below(part),
                        _ => MathBox::leaf("[".into())
                            .beside(part)
                            .beside(MathBox::leaf("]".into())),
                    };
                    (part, end)
                })
            } else {
                braced(source, after).and_then(|(top, next)| {
                    braced(source, next).map(|(bottom, end)| {
                        let top = typeset(top, depth + 1);
                        let bottom = typeset(bottom, depth + 1);
                        let part = if command == r"\binom" {
                            let width = top.width.max(bottom.width).max(1);
                            MathBox {
                                lines: top
                                    .lines
                                    .into_iter()
                                    .chain(bottom.lines)
                                    .map(|line| format!("({})", center(&line, width)))
                                    .collect(),
                                baseline: 0,
                                width: width + 2,
                            }
                        } else {
                            MathBox::fraction(top, bottom)
                        };
                        (part, end)
                    })
                })
            };
            if let Some((box_part, end)) = parsed {
                flush_plain(&mut result, source, plain, at);
                result = result.beside(box_part);
                at = end;
                plain = at;
                continue;
            }
        }
        if let Some(environment) = source[at..].strip_prefix(r"\begin{")
            && let Some(close) = environment.find('}')
        {
            let name = &environment[..close];
            let end_marker = format!(r"\end{{{name}}}");
            let body_start = at + r"\begin{".len() + close + 1;
            if let Some(body_len) = source[body_start..].find(&end_marker) {
                let bounds = match name {
                    "pmatrix" => Some(("(", ")")),
                    "bmatrix" => Some(("[", "]")),
                    "Bmatrix" => Some(("{", "}")),
                    "vmatrix" => Some(("│", "│")),
                    "matrix" | "aligned" | "align" | "align*" | "gather" | "gather*" => {
                        Some(("", ""))
                    }
                    _ => None,
                };
                if let Some((left, right)) = bounds {
                    flush_plain(&mut result, source, plain, at);
                    result = result.beside(MathBox::matrix(
                        &source[body_start..body_start + body_len],
                        left,
                        right,
                        depth,
                    ));
                    at = body_start + body_len + end_marker.len();
                    plain = at;
                    continue;
                }
            }
        }
        if matches!(source.as_bytes()[at], b'^' | b'_') {
            let mut upper = None;
            let mut lower = None;
            let mut end = at;
            for _ in 0..2 {
                let Some(&marker) = source.as_bytes().get(end) else {
                    break;
                };
                if !matches!(marker, b'^' | b'_') {
                    break;
                }
                let Some((value, next)) = argument(source, end + 1) else {
                    break;
                };
                if marker == b'^' {
                    upper = Some(typeset(value, depth + 1));
                } else {
                    lower = Some(typeset(value, depth + 1));
                }
                end = next;
            }
            if end > at {
                flush_plain(&mut result, source, plain, at);
                result = result.beside(MathBox::scripts(upper, lower));
                at = end;
                plain = at;
                continue;
            }
        }
        at += source[at..].chars().next().map_or(1, char::len_utf8);
    }
    flush_plain(&mut result, source, plain, source.len());
    result
}

pub(super) fn text(source: &str) -> String {
    let mut source = source.trim();
    if let Some((_, body)) = source.split_once(r"\begin{document}") {
        source = body
            .split_once(r"\end{document}")
            .map_or(body, |(body, _)| body)
            .trim();
    }
    for (left, right) in [("$$", "$$"), ("$", "$"), (r"\[", r"\]"), (r"\(", r"\)")] {
        if let Some(body) = source
            .strip_prefix(left)
            .and_then(|s| s.strip_suffix(right))
        {
            source = body.trim();
        }
    }
    Parser {
        chars: source.chars().collect(),
        at: 0,
    }
    .sequence(None, 0)
    .trim()
    .to_owned()
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}
impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }
    fn spaces(&mut self) {
        while self.peek().is_some_and(|c| c == ' ' || c == '\t') {
            self.at += 1;
        }
    }
    fn argument(&mut self, depth: usize) -> String {
        self.spaces();
        match self.peek() {
            Some('{') => {
                self.at += 1;
                self.sequence(Some('}'), depth + 1)
            }
            Some('\\') => {
                self.at += 1;
                self.command(depth + 1)
            }
            Some(c) => {
                self.at += 1;
                c.to_string()
            }
            None => String::new(),
        }
    }
    fn sequence(&mut self, end: Option<char>, depth: usize) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            self.at += 1;
            if Some(c) == end {
                break;
            }
            if depth > 32 {
                out.push(c);
                continue;
            }
            match c {
                '\\' => out.push_str(&self.command(depth + 1)),
                '{' => {
                    out.push('(');
                    out.push_str(&self.sequence(Some('}'), depth + 1));
                    out.push(')');
                }
                '^' | '_' => {
                    let value = self.argument(depth + 1);
                    let alphabet = if c == '^' {
                        "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁼⁽⁾ⁿⁱ"
                    } else {
                        "₀₁₂₃₄₅₆₇₈₉₊₋₌₍₎ₙᵢ"
                    };
                    let mapped: Option<String> = value
                        .chars()
                        .map(|c| {
                            "0123456789+-=()ni"
                                .chars()
                                .position(|v| v == c)
                                .and_then(|i| alphabet.chars().nth(i))
                        })
                        .collect();
                    if let Some(mapped) = mapped {
                        out.push_str(&mapped);
                    } else {
                        out.push(c);
                        out.push('(');
                        out.push_str(&value);
                        out.push(')');
                    }
                }
                '&' => out.push_str("  "),
                '$' => {}
                _ => out.push(c),
            }
        }
        out
    }
    fn command(&mut self, depth: usize) -> String {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.at += 1;
        }
        let name: String = self.chars[start..self.at].iter().collect();
        if name.is_empty() {
            return match self.peek() {
                Some('\\') => {
                    self.at += 1;
                    "\n".into()
                }
                Some(',' | ';' | ':' | ' ') => {
                    self.at += 1;
                    " ".into()
                }
                Some('!' | '[' | ']' | '(' | ')') => {
                    self.at += 1;
                    String::new()
                }
                Some(c) => {
                    self.at += 1;
                    c.to_string()
                }
                None => "\\".into(),
            };
        }
        match name.as_str() {
            "frac" | "dfrac" | "tfrac" => {
                let a = self.argument(depth);
                let b = self.argument(depth);
                return format!("({a})/({b})");
            }
            "sqrt" => {
                self.spaces();
                let degree = if self.peek() == Some('[') {
                    self.at += 1;
                    self.sequence(Some(']'), depth + 1)
                } else {
                    String::new()
                };
                let root = match degree.as_str() {
                    "" | "2" => "√".into(),
                    "3" => "∛".into(),
                    "4" => "∜".into(),
                    _ => format!("root[{degree}]"),
                };
                return format!("{root}({})", self.argument(depth));
            }
            "text" | "textrm" | "mathrm" | "mathbf" | "mathit" | "operatorname" | "mathcal"
            | "mathbb" => return self.argument(depth),
            "left" | "right" | "displaystyle" | "textstyle" | "limits" | "nolimits" => {
                return String::new();
            }
            "begin" | "end" => {
                let env = self.argument(depth);
                return match (name.as_str(), env.as_str()) {
                    ("begin", "pmatrix") => "(".into(),
                    ("end", "pmatrix") => ")".into(),
                    ("begin", "bmatrix") => "[".into(),
                    ("end", "bmatrix") => "]".into(),
                    _ => String::new(),
                };
            }
            _ => {}
        }
        let symbol = match name.as_str() {
            "alpha" => "α",
            "beta" => "β",
            "gamma" => "γ",
            "delta" => "δ",
            "epsilon" | "varepsilon" => "ε",
            "zeta" => "ζ",
            "eta" => "η",
            "theta" | "vartheta" => "θ",
            "iota" => "ι",
            "kappa" => "κ",
            "lambda" => "λ",
            "mu" => "μ",
            "nu" => "ν",
            "xi" => "ξ",
            "pi" => "π",
            "rho" => "ρ",
            "sigma" => "σ",
            "tau" => "τ",
            "upsilon" => "υ",
            "phi" | "varphi" => "φ",
            "chi" => "χ",
            "psi" => "ψ",
            "omega" => "ω",
            "Gamma" => "Γ",
            "Delta" => "Δ",
            "Theta" => "Θ",
            "Lambda" => "Λ",
            "Xi" => "Ξ",
            "Pi" => "Π",
            "Sigma" => "Σ",
            "Phi" => "Φ",
            "Psi" => "Ψ",
            "Omega" => "Ω",
            "sum" => "∑",
            "prod" => "∏",
            "int" => "∫",
            "iint" => "∬",
            "oint" => "∮",
            "infty" => "∞",
            "partial" => "∂",
            "nabla" => "∇",
            "times" => "×",
            "cdot" => "·",
            "div" => "÷",
            "pm" => "±",
            "mp" => "∓",
            "le" | "leq" => "≤",
            "ge" | "geq" => "≥",
            "ne" | "neq" => "≠",
            "approx" => "≈",
            "equiv" => "≡",
            "propto" => "∝",
            "to" | "rightarrow" => "→",
            "leftarrow" => "←",
            "Rightarrow" | "implies" => "⇒",
            "Leftrightarrow" | "iff" => "⇔",
            "in" => "∈",
            "notin" => "∉",
            "subset" => "⊂",
            "subseteq" => "⊆",
            "cup" => "∪",
            "cap" => "∩",
            "emptyset" => "∅",
            "forall" => "∀",
            "exists" => "∃",
            "neg" => "¬",
            "land" => "∧",
            "lor" => "∨",
            "ldots" | "dots" => "…",
            "cdots" => "⋯",
            "quad" | "qquad" => "  ",
            "sin" => "sin",
            "cos" => "cos",
            "tan" => "tan",
            "log" => "log",
            "ln" => "ln",
            "lim" => "lim",
            "exp" => "exp",
            "min" => "min",
            "max" => "max",
            _ => return format!("\\{name}"),
        };
        symbol.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readable_formula_fragments_and_scripts() {
        assert_eq!(
            text(r"\alpha_i^2 + \frac{a+1}{b} \leq \sqrt{x}"),
            "αᵢ² + (a+1)/(b) ≤ √(x)"
        );
        assert_eq!(text(r"x^{12} + y_{n+1} + \sqrt[3]{x}"), "x¹² + yₙ₊₁ + ∛(x)");
        assert_eq!(text(r"e^{\alpha+1}"), "e^(α+1)");
    }
    #[test]
    fn wrappers_matrices_and_unknown_commands_are_safe_text() {
        assert_eq!(
            text(r"\documentclass{article}\begin{document}\[\sum_{i=0}^{n} i\]\end{document}"),
            "∑ᵢ₌₀ⁿ i"
        );
        assert_eq!(
            text(r"\begin{pmatrix}a&b\\c&d\end{pmatrix}"),
            "(a  b\nc  d)"
        );
        assert!(text(r"\unknown{x}").contains(r"\unknown"));
    }
}
