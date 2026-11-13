//! Excel formula lexer + Pratt parser, and a printer that can shift relative
//! references (used for fill-down / copy-paste).

use std::fmt::Write as _;
use std::sync::Arc;

use crate::address::{A1, CellRef, MAX_ROWS, Range, col_name, parse_a1, parse_col};
use crate::functions::Func;
use crate::value::ErrorKind;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct RefAddr {
    pub cell: CellRef,
    pub col_abs: bool,
    pub row_abs: bool,
}

impl From<A1> for RefAddr {
    fn from(a: A1) -> Self {
        Self { cell: a.cell, col_abs: a.col_abs, row_abs: a.row_abs }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum RangeKind {
    /// `A1:B5`
    Cells,
    /// `A:C` (whole columns)
    Columns,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl BinOp {
    fn as_str(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Pow => "^",
            BinOp::Concat => "&",
            BinOp::Eq => "=",
            BinOp::Ne => "<>",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
        }
    }

    /// (left bp, right bp). Excel ops are left-associative, including `^`.
    fn binding_power(self) -> (u8, u8) {
        match self {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => (10, 11),
            BinOp::Concat => (20, 21),
            BinOp::Add | BinOp::Sub => (30, 31),
            BinOp::Mul | BinOp::Div => (40, 41),
            BinOp::Pow => (50, 51),
        }
    }
}

const PERCENT_BP: u8 = 60;
const PREFIX_BP: u8 = 70;

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Number(f64),
    Text(Arc<str>),
    Bool(bool),
    Error(ErrorKind),
    Ref(RefAddr),
    Range(RefAddr, RefAddr, RangeKind),
    Neg(Box<Expr>),
    Plus(Box<Expr>),
    Percent(Box<Expr>),
    Paren(Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call(Func, Vec<Expr>),
    UnknownFunc(Arc<str>, Vec<Expr>),
}

impl Expr {
    pub fn range(&self) -> Option<Range> {
        match self {
            Expr::Ref(r) => Some(Range::single(r.cell)),
            Expr::Range(a, b, _) => Some(Range::new(a.cell, b.cell)),
            _ => None,
        }
    }

    /// Visit every cell/range reference in the expression.
    pub fn visit_refs(&self, f: &mut impl FnMut(Range)) {
        match self {
            Expr::Ref(_) | Expr::Range(..) => f(self.range().unwrap()),
            Expr::Neg(e) | Expr::Plus(e) | Expr::Percent(e) | Expr::Paren(e) => e.visit_refs(f),
            Expr::Binary(_, a, b) => {
                a.visit_refs(f);
                b.visit_refs(f);
            }
            Expr::Call(_, args) | Expr::UnknownFunc(_, args) => args.iter().for_each(|a| a.visit_refs(f)),
            Expr::Number(_) | Expr::Text(_) | Expr::Bool(_) | Expr::Error(_) => {}
        }
    }

    pub fn is_volatile(&self) -> bool {
        match self {
            Expr::Call(func, args) => func.is_volatile() || args.iter().any(Expr::is_volatile),
            Expr::UnknownFunc(_, args) => args.iter().any(Expr::is_volatile),
            Expr::Neg(e) | Expr::Plus(e) | Expr::Percent(e) | Expr::Paren(e) => e.is_volatile(),
            Expr::Binary(_, a, b) => a.is_volatile() || b.is_volatile(),
            _ => false,
        }
    }

    /// Print as formula source (without leading `=`), shifting relative refs.
    pub fn to_source_shifted(&self, dr: i64, dc: i64) -> String {
        let mut s = String::new();
        write_expr(self, dr, dc, &mut s);
        s
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

/// Parse a formula body (the text after `=`).
pub fn parse_formula(src: &str) -> Result<Expr, ParseError> {
    let tokens = lex(src)?;
    let mut p = Parser { tokens, pos: 0 };
    let e = p.expr(0)?;
    match p.peek() {
        None => Ok(e),
        Some(t) => Err(ParseError(format!("unexpected {t:?}"))),
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Err(ErrorKind),
    Op(BinOp),
    Minus,
    PlusSign,
    Percent,
    LParen,
    RParen,
    Comma,
    Colon,
}

fn lex(src: &str) -> Result<Vec<Tok>, ParseError> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'0'..=b'9' | b'.' => {
                let s = i;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                    i += 1;
                }
                if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                    let save = i;
                    i += 1;
                    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                        i += 1;
                    }
                    if i < b.len() && b[i].is_ascii_digit() {
                        while i < b.len() && b[i].is_ascii_digit() {
                            i += 1;
                        }
                    } else {
                        i = save;
                    }
                }
                let n: f64 = src[s..i].parse().map_err(|_| ParseError(format!("bad number {}", &src[s..i])))?;
                out.push(Tok::Num(n));
            }
            b'"' => {
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= b.len() {
                        return Err(ParseError("unterminated string".into()));
                    }
                    if b[i] == b'"' {
                        if b.get(i + 1) == Some(&b'"') {
                            s.push('"');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    let ch = src[i..].chars().next().unwrap();
                    s.push(ch);
                    i += ch.len_utf8();
                }
                out.push(Tok::Str(s));
            }
            b'#' => {
                let s = i;
                i += 1;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'/') {
                    i += 1;
                }
                if i < b.len() && (b[i] == b'!' || b[i] == b'?') {
                    i += 1;
                }
                let e = ErrorKind::parse(&src[s..i]).ok_or_else(|| ParseError(format!("unknown error {}", &src[s..i])))?;
                out.push(Tok::Err(e));
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'.' | b'$')) {
                    i += 1;
                }
                out.push(Tok::Ident(src[s..i].to_string()));
            }
            b'+' => {
                out.push(Tok::PlusSign);
                i += 1;
            }
            b'-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            b'*' => {
                out.push(Tok::Op(BinOp::Mul));
                i += 1;
            }
            b'/' => {
                out.push(Tok::Op(BinOp::Div));
                i += 1;
            }
            b'^' => {
                out.push(Tok::Op(BinOp::Pow));
                i += 1;
            }
            b'&' => {
                out.push(Tok::Op(BinOp::Concat));
                i += 1;
            }
            b'%' => {
                out.push(Tok::Percent);
                i += 1;
            }
            b'=' => {
                out.push(Tok::Op(BinOp::Eq));
                i += 1;
            }
            b'<' => {
                let (op, n) = match b.get(i + 1) {
                    Some(b'=') => (BinOp::Le, 2),
                    Some(b'>') => (BinOp::Ne, 2),
                    _ => (BinOp::Lt, 1),
                };
                out.push(Tok::Op(op));
                i += n;
            }
            b'>' => {
                let (op, n) = if b.get(i + 1) == Some(&b'=') { (BinOp::Ge, 2) } else { (BinOp::Gt, 1) };
                out.push(Tok::Op(op));
                i += n;
            }
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b',' | b';' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b':' => {
                out.push(Tok::Colon);
                i += 1;
            }
            _ => {
                let ch = src[i..].chars().next().unwrap();
                return Err(ParseError(format!("unexpected character '{ch}'")));
            }
        }
    }
    Ok(out)
}

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn expect(&mut self, t: Tok) -> Result<(), ParseError> {
        match self.next() {
            Some(x) if x == t => Ok(()),
            other => Err(ParseError(format!("expected {t:?}, found {other:?}"))),
        }
    }

    fn expr(&mut self, min_bp: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.prefix()?;
        loop {
            match self.peek() {
                Some(Tok::Percent) if PERCENT_BP >= min_bp => {
                    self.pos += 1;
                    lhs = Expr::Percent(Box::new(lhs));
                }
                Some(Tok::Op(_) | Tok::Minus | Tok::PlusSign) => {
                    let op = match self.peek().unwrap() {
                        Tok::Op(op) => *op,
                        Tok::Minus => BinOp::Sub,
                        _ => BinOp::Add,
                    };
                    let (l, r) = op.binding_power();
                    if l < min_bp {
                        break;
                    }
                    self.pos += 1;
                    let rhs = self.expr(r)?;
                    lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    fn prefix(&mut self) -> Result<Expr, ParseError> {
        match self.next() {
            Some(Tok::Num(n)) => Ok(Expr::Number(n)),
            Some(Tok::Str(s)) => Ok(Expr::Text(s.into())),
            Some(Tok::Err(e)) => Ok(Expr::Error(e)),
            Some(Tok::Minus) => Ok(Expr::Neg(Box::new(self.expr(PREFIX_BP)?))),
            Some(Tok::PlusSign) => Ok(Expr::Plus(Box::new(self.expr(PREFIX_BP)?))),
            Some(Tok::LParen) => {
                let e = self.expr(0)?;
                self.expect(Tok::RParen)?;
                Ok(Expr::Paren(Box::new(e)))
            }
            Some(Tok::Ident(id)) => self.ident(id),
            other => Err(ParseError(format!("unexpected {}", match other {
                Some(t) => format!("{t:?}"),
                None => "end of formula".into(),
            }))),
        }
    }

    fn ident(&mut self, id: String) -> Result<Expr, ParseError> {
        // Function call
        if self.peek() == Some(&Tok::LParen) {
            self.pos += 1;
            let mut args = Vec::new();
            if self.peek() != Some(&Tok::RParen) {
                loop {
                    args.push(self.expr(0)?);
                    match self.next() {
                        Some(Tok::Comma) => continue,
                        Some(Tok::RParen) => break,
                        other => return Err(ParseError(format!("expected , or ) found {other:?}"))),
                    }
                }
            } else {
                self.pos += 1;
            }
            return Ok(match Func::from_name(&id) {
                Some(f) => Expr::Call(f, args),
                None => Expr::UnknownFunc(id.to_ascii_uppercase().into(), args),
            });
        }
        // Cell reference or range
        if let Some(a) = parse_a1(&id) {
            if self.peek() == Some(&Tok::Colon) {
                self.pos += 1;
                match self.next() {
                    Some(Tok::Ident(id2)) => {
                        let b = parse_a1(&id2).ok_or_else(|| ParseError(format!("bad range end {id2}")))?;
                        return Ok(Expr::Range(a.into(), b.into(), RangeKind::Cells));
                    }
                    other => return Err(ParseError(format!("bad range end {other:?}"))),
                }
            }
            return Ok(Expr::Ref(a.into()));
        }
        // Whole-column range A:C / $A:$C
        if let Some(c1) = parse_col_abs(&id)
            && self.peek() == Some(&Tok::Colon)
        {
            self.pos += 1;
            if let Some(Tok::Ident(id2)) = self.next()
                && let Some(c2) = parse_col_abs(&id2)
            {
                let a = RefAddr { cell: CellRef::new(0, c1.0), col_abs: c1.1, row_abs: true };
                let b = RefAddr { cell: CellRef::new(MAX_ROWS - 1, c2.0), col_abs: c2.1, row_abs: true };
                return Ok(Expr::Range(a, b, RangeKind::Columns));
            }
            return Err(ParseError(format!("bad column range after {id}:")));
        }
        match id.to_ascii_uppercase().as_str() {
            "TRUE" => Ok(Expr::Bool(true)),
            "FALSE" => Ok(Expr::Bool(false)),
            _ => Ok(Expr::Error(ErrorKind::Name)),
        }
    }
}

fn parse_col_abs(s: &str) -> Option<(u32, bool)> {
    match s.strip_prefix('$') {
        Some(rest) => parse_col(rest).map(|c| (c, true)),
        None => parse_col(s).map(|c| (c, false)),
    }
}

fn shift_ref(r: &RefAddr, dr: i64, dc: i64) -> Option<RefAddr> {
    let row = if r.row_abs { r.cell.row as i64 } else { r.cell.row as i64 + dr };
    let col = if r.col_abs { r.cell.col as i64 } else { r.cell.col as i64 + dc };
    let cell = CellRef::new(0, 0).offset(row, col)?;
    Some(RefAddr { cell, ..*r })
}

fn write_ref(r: &RefAddr, out: &mut String) {
    let _ = write!(
        out,
        "{}{}{}{}",
        if r.col_abs { "$" } else { "" },
        col_name(r.cell.col),
        if r.row_abs { "$" } else { "" },
        r.cell.row + 1
    );
}

fn write_expr(e: &Expr, dr: i64, dc: i64, out: &mut String) {
    match e {
        Expr::Number(n) => {
            let _ = write!(out, "{n}");
        }
        Expr::Text(s) => {
            out.push('"');
            out.push_str(&s.replace('"', "\"\""));
            out.push('"');
        }
        Expr::Bool(b) => out.push_str(if *b { "TRUE" } else { "FALSE" }),
        Expr::Error(err) => out.push_str(err.as_str()),
        Expr::Ref(r) => match shift_ref(r, dr, dc) {
            Some(r) => write_ref(&r, out),
            None => out.push_str("#REF!"),
        },
        Expr::Range(a, b, kind) => {
            let (sa, sb) = match kind {
                RangeKind::Cells => (shift_ref(a, dr, dc), shift_ref(b, dr, dc)),
                RangeKind::Columns => (shift_ref(a, 0, dc), shift_ref(b, 0, dc)),
            };
            match (sa, sb, kind) {
                (Some(a), Some(b), RangeKind::Cells) => {
                    write_ref(&a, out);
                    out.push(':');
                    write_ref(&b, out);
                }
                (Some(a), Some(b), RangeKind::Columns) => {
                    let _ = write!(
                        out,
                        "{}{}:{}{}",
                        if a.col_abs { "$" } else { "" },
                        col_name(a.cell.col),
                        if b.col_abs { "$" } else { "" },
                        col_name(b.cell.col)
                    );
                }
                _ => out.push_str("#REF!"),
            }
        }
        Expr::Neg(x) => {
            out.push('-');
            write_expr(x, dr, dc, out);
        }
        Expr::Plus(x) => {
            out.push('+');
            write_expr(x, dr, dc, out);
        }
        Expr::Percent(x) => {
            write_expr(x, dr, dc, out);
            out.push('%');
        }
        Expr::Paren(x) => {
            out.push('(');
            write_expr(x, dr, dc, out);
            out.push(')');
        }
        Expr::Binary(op, a, b) => {
            write_expr(a, dr, dc, out);
            out.push_str(op.as_str());
            write_expr(b, dr, dc, out);
        }
        Expr::Call(_, args) | Expr::UnknownFunc(_, args) => {
            let name = match e {
                Expr::Call(f, _) => f.name(),
                Expr::UnknownFunc(n, _) => n,
                _ => unreachable!(),
            };
            out.push_str(name);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_expr(a, dr, dc, out);
            }
            out.push(')');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(s: &str) -> String {
        parse_formula(s).unwrap().to_source_shifted(0, 0)
    }

    #[test]
    fn parses_and_prints() {
        assert_eq!(roundtrip("1+2*3"), "1+2*3");
        assert_eq!(roundtrip("SUM( A1:B2 , $C$3)"), "SUM(A1:B2,$C$3)");
        assert_eq!(roundtrip("-(A1)%"), "-(A1)%");
        assert_eq!(roundtrip("\"a\"\"b\"&A1"), "\"a\"\"b\"&A1");
        assert_eq!(roundtrip("sum(a:a)"), "SUM(A:A)");
        assert_eq!(roundtrip("A1<>B1"), "A1<>B1");
    }

    #[test]
    fn precedence() {
        // -2^2 = 4 in Excel: negation binds tighter than ^
        let e = parse_formula("-2^2").unwrap();
        assert!(matches!(e, Expr::Binary(BinOp::Pow, ..)));
        let e = parse_formula("1+2*3").unwrap();
        assert!(matches!(e, Expr::Binary(BinOp::Add, ..)));
        let e = parse_formula("1&2=3").unwrap();
        assert!(matches!(e, Expr::Binary(BinOp::Eq, ..)));
    }

    #[test]
    fn shifting() {
        let e = parse_formula("A1+$B$1+C$1+$D1+SUM(A1:A3)").unwrap();
        assert_eq!(e.to_source_shifted(2, 1), "B3+$B$1+D$1+$D3+SUM(B3:B5)");
        let e = parse_formula("A1").unwrap();
        assert_eq!(e.to_source_shifted(-1, 0), "#REF!");
    }

    #[test]
    fn errors() {
        assert!(parse_formula("1+").is_err());
        assert!(parse_formula("SUM(1,2").is_err());
        assert!(parse_formula("\"abc").is_err());
    }
}
