//! Tokeniser and recursive-descent parser for the subset in `mod.rs`.

use super::{AggOp, BinOp, Expr, MatchOp, Matcher, RangeFn, Selector};
use anyhow::{Context, Result, anyhow, bail};
use regex_lite::Regex;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    String(String),
    Number(f64),
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Eq,
    Ne,
    Re,
    NotRe,
    /// `==`, which compares, as opposed to `=`, which matches a label.
    EqEq,
    Gt,
    Lt,
    Ge,
    Le,
    Plus,
    Minus,
    Star,
    Slash,
}

fn lex(input: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            i += 1;
            continue;
        }

        // Identifiers double as metric names, label names, function names
        // and keywords; `__name__` means leading underscores count.
        if c.is_ascii_alphabetic() || c == '_' || c == ':' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == ':')
            {
                i += 1;
            }
            out.push(Token::Ident(chars[start..i].iter().collect()));
            continue;
        }

        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            // `1e6`, but not the `d` of a `[30d]` duration.
            let digit_at = |j: usize| chars.get(j).is_some_and(|c| c.is_ascii_digit());
            if matches!(chars.get(i), Some('e' | 'E')) {
                let sign = usize::from(matches!(chars.get(i + 1), Some('+' | '-')));
                if digit_at(i + 1 + sign) {
                    i += 1 + sign;
                    while digit_at(i) {
                        i += 1;
                    }
                }
            }
            let text: String = chars[start..i].iter().collect();
            out.push(Token::Number(text.parse().context("bad number")?));
            continue;
        }

        if c == '"' || c == '\'' {
            let quote = c;
            i += 1;
            let mut value = String::new();
            while i < chars.len() && chars[i] != quote {
                // Lens escapes regex metacharacters in ingress queries
                // (`^2\\d*`), so backslash escapes have to survive.
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                    match chars[i] {
                        'n' => value.push('\n'),
                        't' => value.push('\t'),
                        other => {
                            if other != '"' && other != '\'' && other != '\\' {
                                value.push('\\');
                            }
                            value.push(other);
                        }
                    }
                } else {
                    value.push(chars[i]);
                }
                i += 1;
            }
            if i >= chars.len() {
                bail!("unterminated string literal");
            }
            i += 1;
            out.push(Token::String(value));
            continue;
        }

        let (token, width) = match (c, chars.get(i + 1)) {
            ('=', Some('~')) => (Token::Re, 2),
            ('!', Some('~')) => (Token::NotRe, 2),
            ('!', Some('=')) => (Token::Ne, 2),
            ('=', Some('=')) => (Token::EqEq, 2),
            ('>', Some('=')) => (Token::Ge, 2),
            ('<', Some('=')) => (Token::Le, 2),
            ('>', _) => (Token::Gt, 1),
            ('<', _) => (Token::Lt, 1),
            ('=', _) => (Token::Eq, 1),
            ('(', _) => (Token::LParen, 1),
            (')', _) => (Token::RParen, 1),
            ('{', _) => (Token::LBrace, 1),
            ('}', _) => (Token::RBrace, 1),
            ('[', _) => (Token::LBracket, 1),
            (']', _) => (Token::RBracket, 1),
            (',', _) => (Token::Comma, 1),
            ('+', _) => (Token::Plus, 1),
            ('-', _) => (Token::Minus, 1),
            ('*', _) => (Token::Star, 1),
            ('/', _) => (Token::Slash, 1),
            _ => bail!("unexpected character {c:?}"),
        };
        out.push(token);
        i += width;
    }

    Ok(out)
}

pub fn parse(input: &str) -> Result<Expr> {
    let tokens = lex(input)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        selectors: 0,
    };
    let expr = parser.parse_expr(0)?;
    if parser.pos != parser.tokens.len() {
        bail!("trailing tokens after expression");
    }
    Ok(expr)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// How many selectors have been parsed: the next one's id.
    selectors: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn expect(&mut self, want: Token) -> Result<()> {
        match self.next() {
            Some(got) if got == want => Ok(()),
            other => bail!("expected {want:?}, found {other:?}"),
        }
    }

    /// Precedence climbing, loosest first as in Prometheus: `or`, then
    /// `and`/`unless`, comparisons, `+`/`-`, and `*`/`/`.
    fn parse_expr(&mut self, min_bp: u8) -> Result<Expr> {
        let mut lhs = self.parse_atom()?;

        while let Some(token) = self.peek() {
            let (op, bp) = match token {
                Token::Ident(word) if word == "or" => (BinOp::Or, 1),
                Token::Ident(word) if word == "and" => (BinOp::And, 2),
                Token::Ident(word) if word == "unless" => (BinOp::Unless, 2),
                Token::EqEq => (BinOp::Eq, 3),
                Token::Ne => (BinOp::Ne, 3),
                Token::Gt => (BinOp::Gt, 3),
                Token::Lt => (BinOp::Lt, 3),
                Token::Ge => (BinOp::Ge, 3),
                Token::Le => (BinOp::Le, 3),
                Token::Plus => (BinOp::Add, 4),
                Token::Minus => (BinOp::Sub, 4),
                Token::Star => (BinOp::Mul, 5),
                Token::Slash => (BinOp::Div, 5),
                _ => break,
            };
            if bp < min_bp {
                break;
            }
            self.pos += 1;
            let rhs = self.parse_expr(bp + 1)?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }

        Ok(lhs)
    }

    fn parse_atom(&mut self) -> Result<Expr> {
        match self.next() {
            Some(Token::LParen) => {
                let inner = self.parse_expr(0)?;
                self.expect(Token::RParen)?;
                Ok(inner)
            }
            Some(Token::Number(n)) => Ok(Expr::Number(n)),
            // `-x` is `0 - x`; only ever a sign on a number or a series.
            Some(Token::Minus) => Ok(Expr::Binary {
                op: BinOp::Sub,
                lhs: Box::new(Expr::Number(0.0)),
                rhs: Box::new(self.parse_atom()?),
            }),
            // A bare `{...}` selector, as in the podUsage query.
            Some(Token::LBrace) => {
                let matchers = self.parse_matchers()?;
                Ok(Expr::Selector(self.selector(None, matchers)))
            }
            Some(Token::Ident(name)) => {
                // A word followed by `(` is a call; otherwise it is a
                // metric name, even one spelled like a function.
                let call = self.peek() == Some(&Token::LParen)
                    || self.peek() == Some(&Token::Ident("by".into()))
                    || self.peek() == Some(&Token::Ident("without".into()));
                match name.as_str() {
                    "sum" if call => self.parse_agg(AggOp::Sum),
                    "avg" if call => self.parse_agg(AggOp::Avg),
                    "min" if call => self.parse_agg(AggOp::Min),
                    "max" if call => self.parse_agg(AggOp::Max),
                    "count" if call => self.parse_agg(AggOp::Count),
                    "rate" if call => self.parse_range(RangeFn::Rate),
                    "irate" if call => self.parse_range(RangeFn::Irate),
                    "increase" if call => self.parse_range(RangeFn::Increase),
                    "clamp_min" | "clamp_max" if call => {
                        self.pos += 1;
                        let expr = self.parse_expr(0)?;
                        self.expect(Token::Comma)?;
                        let bound = self.parse_expr(0)?;
                        self.expect(Token::RParen)?;
                        Ok(Expr::Clamp {
                            expr: Box::new(expr),
                            bound: Box::new(bound),
                            min: name == "clamp_min",
                        })
                    }
                    "vector" if call => {
                        self.pos += 1;
                        let inner = self.parse_expr(0)?;
                        self.expect(Token::RParen)?;
                        Ok(Expr::Vector(Box::new(inner)))
                    }
                    "time" if call => {
                        self.pos += 1;
                        self.expect(Token::RParen)?;
                        Ok(Expr::Time)
                    }
                    _ if call => bail!("unsupported function {name}()"),
                    _ => {
                        let matchers = if self.peek() == Some(&Token::LBrace) {
                            self.pos += 1;
                            self.parse_matchers()?
                        } else {
                            Vec::new()
                        };
                        Ok(Expr::Selector(self.selector(Some(name), matchers)))
                    }
                }
            }
            other => bail!("unexpected token {other:?}"),
        }
    }

    fn parse_agg(&mut self, op: AggOp) -> Result<Expr> {
        // `sum by (x) (expr)` and `sum(expr) by (x)` are both legal
        // PromQL. Lens emits the second, Grafana users the first.
        let leading = self.take_by()?;
        self.expect(Token::LParen)?;
        let inner = self.parse_expr(0)?;
        self.expect(Token::RParen)?;
        let (by, without) = match leading {
            Some(clause) => (Some(clause.0), clause.1),
            None => match self.take_by()? {
                Some(clause) => (Some(clause.0), clause.1),
                None => (None, false),
            },
        };
        Ok(Expr::Agg {
            op,
            expr: Box::new(inner),
            by,
            without,
        })
    }

    /// A `by (...)` or `without (...)` clause: the labels, and whether it
    /// was `without`.
    fn take_by(&mut self) -> Result<Option<(Vec<String>, bool)>> {
        let without = match self.peek() {
            Some(Token::Ident(word)) if word == "by" => false,
            Some(Token::Ident(word)) if word == "without" => true,
            _ => return Ok(None),
        };
        self.pos += 1;
        self.expect(Token::LParen)?;
        let mut labels = Vec::new();
        loop {
            match self.next() {
                Some(Token::Ident(label)) => labels.push(label),
                Some(Token::RParen) => break,
                other => bail!("expected label in by(), found {other:?}"),
            }
            match self.next() {
                Some(Token::Comma) => continue,
                Some(Token::RParen) => break,
                other => bail!("expected , or ) in by(), found {other:?}"),
            }
        }
        Ok(Some((labels, without)))
    }

    fn parse_range(&mut self, func: RangeFn) -> Result<Expr> {
        self.expect(Token::LParen)?;
        let (name, matchers) = match self.next() {
            Some(Token::Ident(name)) => {
                let matchers = if self.peek() == Some(&Token::LBrace) {
                    self.pos += 1;
                    self.parse_matchers()?
                } else {
                    Vec::new()
                };
                (Some(name), matchers)
            }
            Some(Token::LBrace) => (None, self.parse_matchers()?),
            other => bail!("{func:?} expects a selector, found {other:?}"),
        };
        let window = self.parse_window()?;
        self.expect(Token::RParen)?;
        Ok(Expr::Range {
            func,
            selector: self.selector(name, matchers),
            window,
        })
    }

    /// Folds an `__name__="x"` matcher into the selector's name field so
    /// the store can use its index. A regex on `__name__` stays a matcher.
    fn selector(&mut self, name: Option<String>, matchers: Vec<Matcher>) -> Selector {
        let mut name = name;
        let mut kept = Vec::with_capacity(matchers.len());

        for matcher in matchers {
            if matcher.label == crate::store::NAME_LABEL && matcher.op == MatchOp::Eq {
                name = Some(matcher.value);
                continue;
            }
            kept.push(matcher);
        }

        self.selectors += 1;
        Selector {
            name,
            matchers: kept,
            id: self.selectors - 1,
        }
    }

    fn parse_window(&mut self) -> Result<i64> {
        self.expect(Token::LBracket)?;
        // The lexer splits `1m` into a number and an identifier.
        let value = match self.next() {
            Some(Token::Number(n)) => n,
            other => bail!("expected a range duration, found {other:?}"),
        };
        let unit = match self.next() {
            Some(Token::Ident(unit)) => unit,
            other => bail!("expected a duration unit, found {other:?}"),
        };
        self.expect(Token::RBracket)?;
        let seconds = match unit.as_str() {
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            "d" => 86400.0,
            other => bail!("unsupported duration unit {other:?}"),
        };
        Ok((value * seconds) as i64)
    }

    /// Consumes matchers up to and including the closing brace.
    fn parse_matchers(&mut self) -> Result<Vec<Matcher>> {
        let mut out = Vec::new();

        if self.peek() == Some(&Token::RBrace) {
            self.pos += 1;
            return Ok(out);
        }

        loop {
            let label = match self.next() {
                Some(Token::Ident(label)) => label,
                other => bail!("expected a label name, found {other:?}"),
            };
            let op = match self.next() {
                Some(Token::Eq) => MatchOp::Eq,
                Some(Token::Ne) => MatchOp::Ne,
                Some(Token::Re) => MatchOp::Re,
                Some(Token::NotRe) => MatchOp::NotRe,
                other => bail!("expected a match operator, found {other:?}"),
            };
            let value = match self.next() {
                Some(Token::String(value)) => value,
                other => bail!("expected a quoted matcher value, found {other:?}"),
            };

            let regex = match op {
                MatchOp::Re | MatchOp::NotRe => Some(
                    Regex::new(&format!("^(?:{value})$"))
                        .map_err(|e| anyhow!("bad regex {value:?}: {e}"))?,
                ),
                _ => None,
            };
            out.push(Matcher {
                label,
                op,
                value,
                regex,
            });

            match self.next() {
                Some(Token::Comma) => continue,
                Some(Token::RBrace) => break,
                other => bail!("expected , or }} in selector, found {other:?}"),
            }
        }

        Ok(out)
    }
}
