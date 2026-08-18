//! Tokeniser and recursive-descent parser for the subset in `mod.rs`.

use super::{BinOp, Expr, MatchOp, Matcher, Selector};
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
            ('=', Some('=')) => (Token::Eq, 2),
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
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.parse_expr(0)?;
    if parser.pos != parser.tokens.len() {
        bail!("trailing tokens after expression");
    }
    Ok(expr)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
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

    /// Precedence climbing. `+`/`-` bind at 1, `*`/`/` at 2 -- enough for
    /// `A - (B + C)` and for the division Lens uses in a few ratios.
    fn parse_expr(&mut self, min_bp: u8) -> Result<Expr> {
        let mut lhs = self.parse_atom()?;

        while let Some(token) = self.peek() {
            let (op, bp) = match token {
                Token::Plus => (BinOp::Add, 1),
                Token::Minus => (BinOp::Sub, 1),
                Token::Star => (BinOp::Mul, 2),
                Token::Slash => (BinOp::Div, 2),
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
            // A bare `{...}` selector, as in the podUsage query.
            Some(Token::LBrace) => {
                let matchers = self.parse_matchers()?;
                Ok(Expr::Selector(selector_from(None, matchers)))
            }
            Some(Token::Ident(name)) => match name.as_str() {
                "sum" => self.parse_sum(),
                "rate" | "irate" => self.parse_rate(),
                _ => {
                    let matchers = if self.peek() == Some(&Token::LBrace) {
                        self.pos += 1;
                        self.parse_matchers()?
                    } else {
                        Vec::new()
                    };
                    Ok(Expr::Selector(selector_from(Some(name), matchers)))
                }
            },
            other => bail!("unexpected token {other:?}"),
        }
    }

    fn parse_sum(&mut self) -> Result<Expr> {
        // `sum by (x) (expr)` and `sum(expr) by (x)` are both legal
        // PromQL. Lens emits the second, but accepting both costs four
        // lines and removes a way for this to break later.
        let leading_by = self.take_by()?;
        self.expect(Token::LParen)?;
        let inner = self.parse_expr(0)?;
        self.expect(Token::RParen)?;
        let by = match leading_by {
            Some(by) => Some(by),
            None => self.take_by()?,
        };
        Ok(Expr::Sum {
            expr: Box::new(inner),
            by,
        })
    }

    fn take_by(&mut self) -> Result<Option<Vec<String>>> {
        if self.peek() != Some(&Token::Ident("by".to_string())) {
            return Ok(None);
        }
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
        Ok(Some(labels))
    }

    fn parse_rate(&mut self) -> Result<Expr> {
        self.expect(Token::LParen)?;
        let name = match self.next() {
            Some(Token::Ident(name)) => Some(name),
            Some(Token::LBrace) => {
                let matchers = self.parse_matchers()?;
                let window = self.parse_window()?;
                self.expect(Token::RParen)?;
                return Ok(Expr::Rate(selector_from(None, matchers), window));
            }
            other => bail!("rate() expects a selector, found {other:?}"),
        };
        let matchers = if self.peek() == Some(&Token::LBrace) {
            self.pos += 1;
            self.parse_matchers()?
        } else {
            Vec::new()
        };
        let window = self.parse_window()?;
        self.expect(Token::RParen)?;
        Ok(Expr::Rate(selector_from(name, matchers), window))
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

/// Folds an `__name__="x"` matcher into the selector's name field so the
/// store can use its index. A regex on `__name__` stays a matcher.
fn selector_from(name: Option<String>, matchers: Vec<Matcher>) -> Selector {
    let mut name = name;
    let mut kept = Vec::with_capacity(matchers.len());

    for matcher in matchers {
        if matcher.label == crate::store::NAME_LABEL && matcher.op == MatchOp::Eq {
            name = Some(matcher.value);
            continue;
        }
        kept.push(matcher);
    }

    Selector {
        name,
        matchers: kept,
    }
}
