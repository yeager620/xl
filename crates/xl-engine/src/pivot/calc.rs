//! Calculated-field formulas: arithmetic over other fields' names, like
//! Excel's `=Revenue-Cost` or `='Unit Price'*Units*0.1`. Each name stands for
//! the sum of that field in the group being computed.

use crate::value::ErrorKind;

#[derive(Clone, Debug, PartialEq)]
pub enum CalcExpr {
    Number(f64),
    Field(String),
    Neg(Box<CalcExpr>),
    Percent(Box<CalcExpr>),
    Bin(char, Box<CalcExpr>, Box<CalcExpr>),
}

impl CalcExpr {
    /// Parse a formula (with or without the leading `=`).
    pub fn parse(src: &str) -> Result<CalcExpr, String> {
        let src = src.trim();
        let src = src.strip_prefix('=').unwrap_or(src);
        let mut p = Parser { s: src.as_bytes(), i: 0, text: src };
        let e = p.expr()?;
        p.ws();
        if p.i < p.s.len() {
            return Err(format!("Unexpected “{}” in the formula", &src[p.i..]));
        }
        Ok(e)
    }

    /// The field names it uses, in order of appearance.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        self.walk(&mut |e| {
            if let CalcExpr::Field(n) = e
                && !out.iter().any(|o| o.eq_ignore_ascii_case(n))
            {
                out.push(n.clone());
            }
        });
        out
    }

    fn walk(&self, f: &mut dyn FnMut(&CalcExpr)) {
        f(self);
        match self {
            CalcExpr::Neg(a) | CalcExpr::Percent(a) => a.walk(f),
            CalcExpr::Bin(_, a, b) => {
                a.walk(f);
                b.walk(f);
            }
            _ => {}
        }
    }

    /// Evaluate with `field` giving each name's value (None: unknown name).
    pub fn eval(&self, field: &dyn Fn(&str) -> Option<f64>) -> Result<f64, ErrorKind> {
        Ok(match self {
            CalcExpr::Number(n) => *n,
            CalcExpr::Field(name) => field(name).ok_or(ErrorKind::Name)?,
            CalcExpr::Neg(a) => -a.eval(field)?,
            CalcExpr::Percent(a) => a.eval(field)? / 100.0,
            CalcExpr::Bin(op, a, b) => {
                let (x, y) = (a.eval(field)?, b.eval(field)?);
                let r = match op {
                    '+' => x + y,
                    '-' => x - y,
                    '*' => x * y,
                    '/' if y == 0.0 => return Err(ErrorKind::Div0),
                    '/' => x / y,
                    _ => x.powf(y),
                };
                if !r.is_finite() {
                    return Err(ErrorKind::Num);
                }
                r
            }
        })
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    text: &'a str,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }

    fn expr(&mut self) -> Result<CalcExpr, String> {
        let mut a = self.term()?;
        while let Some(c @ (b'+' | b'-')) = self.peek() {
            self.i += 1;
            a = CalcExpr::Bin(c as char, Box::new(a), Box::new(self.term()?));
        }
        Ok(a)
    }

    fn term(&mut self) -> Result<CalcExpr, String> {
        let mut a = self.power()?;
        while let Some(c @ (b'*' | b'/')) = self.peek() {
            self.i += 1;
            a = CalcExpr::Bin(c as char, Box::new(a), Box::new(self.power()?));
        }
        Ok(a)
    }

    fn power(&mut self) -> Result<CalcExpr, String> {
        let mut a = self.unary()?;
        while self.peek() == Some(b'^') {
            self.i += 1;
            a = CalcExpr::Bin('^', Box::new(a), Box::new(self.unary()?));
        }
        Ok(a)
    }

    fn unary(&mut self) -> Result<CalcExpr, String> {
        match self.peek() {
            Some(b'-') => {
                self.i += 1;
                Ok(CalcExpr::Neg(Box::new(self.unary()?)))
            }
            Some(b'+') => {
                self.i += 1;
                self.unary()
            }
            _ => {
                let mut a = self.primary()?;
                while self.peek() == Some(b'%') {
                    self.i += 1;
                    a = CalcExpr::Percent(Box::new(a));
                }
                Ok(a)
            }
        }
    }

    fn primary(&mut self) -> Result<CalcExpr, String> {
        match self.peek() {
            None => Err("The formula ends too soon".into()),
            Some(b'(') => {
                self.i += 1;
                let e = self.expr()?;
                if self.peek() != Some(b')') {
                    return Err("A “)” is missing".into());
                }
                self.i += 1;
                Ok(e)
            }
            Some(b'\'') => {
                // 'Field name' ('' for a quote inside).
                self.i += 1;
                let mut name = String::new();
                loop {
                    let rest = &self.text[self.i..];
                    let Some(q) = rest.find('\'') else { return Err("A closing ' is missing".into()) };
                    name.push_str(&rest[..q]);
                    self.i += q + 1;
                    if self.s.get(self.i) == Some(&b'\'') {
                        name.push('\'');
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                Ok(CalcExpr::Field(name))
            }
            Some(c) if c.is_ascii_digit() || c == b'.' => {
                let start = self.i;
                while self.i < self.s.len() && (self.s[self.i].is_ascii_digit() || self.s[self.i] == b'.') {
                    self.i += 1;
                }
                // Exponent: 1e3, 2.5E-2.
                if self.i < self.s.len() && (self.s[self.i] | 0x20) == b'e' {
                    let save = self.i;
                    self.i += 1;
                    if self.i < self.s.len() && matches!(self.s[self.i], b'+' | b'-') {
                        self.i += 1;
                    }
                    if self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                            self.i += 1;
                        }
                    } else {
                        self.i = save;
                    }
                }
                self.text[start..self.i].parse().map(CalcExpr::Number).map_err(|_| format!("“{}” isn't a number", &self.text[start..self.i]))
            }
            Some(_) => {
                let start = self.i;
                for (off, ch) in self.text[start..].char_indices() {
                    if ch.is_alphanumeric() || ch == '_' || ch == '.' {
                        self.i = start + off + ch.len_utf8();
                    } else {
                        break;
                    }
                }
                if self.i == start {
                    return Err(format!("Unexpected “{}” in the formula", &self.text[start..]));
                }
                Ok(CalcExpr::Field(self.text[start..self.i].to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_evaluates() {
        let e = CalcExpr::parse("=Revenue - Cost*2 + 'Unit Price'/4 - -1 + 50%").unwrap();
        assert_eq!(e.names(), vec!["Revenue", "Cost", "Unit Price"]);
        let v = e.eval(&|n| match n {
            "Revenue" => Some(100.0),
            "Cost" => Some(10.0),
            "Unit Price" => Some(8.0),
            _ => None,
        });
        assert_eq!(v, Ok(100.0 - 20.0 + 2.0 + 1.0 + 0.5));
        assert_eq!(CalcExpr::parse("2^3^2").unwrap().eval(&|_| None), Ok(64.0));
        assert_eq!(CalcExpr::parse("a/b").unwrap().eval(&|_| Some(0.0)), Err(ErrorKind::Div0));
        assert_eq!(CalcExpr::parse("x").unwrap().eval(&|_| None), Err(ErrorKind::Name));
        assert!(CalcExpr::parse("(1+2").is_err());
        assert!(CalcExpr::parse("1 +").is_err());
        assert!(CalcExpr::parse("1 2").is_err());
    }
}
