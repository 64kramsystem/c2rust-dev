use std::collections::HashMap;
use std::str;
use std::str::FromStr;
use syntax::ast::*;
use syntax::source_map::DUMMY_SP;
use syntax::ptr::P;
use syntax::parse::token::{Token, Nonterminal};
use syntax::tokenstream::TokenTree;

use crate::api::*;
use crate::command::{CommandState, Registry};
use crate::driver;
use crate::transform::Transform;


/// # `convert_format_args` Command
/// 
/// Usage: `convert_format_args`
/// 
/// Marks: `target`
/// 
/// For each function call, if one of its argument expressions is marked `target`,
/// then parse that argument as a `printf` format string, with the subsequent arguments as the
/// format args.  Replace both the format string and the args with an invocation of the Rust
/// `format_args!` macro.
/// 
/// This transformation applies casts to the remaining arguments to account for differences in
/// argument conversion behavior between C-style and Rust-style string formatting.  However, it
/// does not attempt to convert the `format_args!` output into something compatible with the
/// original C function.  This results in a type error, so this pass should usually be followed up
/// by an additional rewrite to change the function being called.
/// 
/// Example:
/// 
///     printf("hello %d\n", 123);
/// 
/// If the string `"hello %d\n"` is marked `target`, then running
/// `convert_format_string` will replace this call with
/// 
///     printf(format_args!("hello {:}\n", 123 as i32));
/// 
/// At this point, it would be wise to replace the `printf` expression with a function that accepts
/// the `std::fmt::Arguments` produced by `format_args!`.
pub struct ConvertFormatArgs;

impl Transform for ConvertFormatArgs {
    fn transform(&self, krate: Crate, st: &CommandState, _cx: &driver::Ctxt) -> Crate {
        fold_nodes(krate, |e: P<Expr>| {
            let fmt_idx = match e.node {
                ExprKind::Call(_, ref args) =>
                    args.iter().position(|e| st.marked(e.id, "target")),
                _ => None,
            };
            if fmt_idx.is_none() {
                return e;
            }
            let fmt_idx = fmt_idx.unwrap();


            let (func, args) = expect!([e.node] ExprKind::Call(ref f, ref a) => (f, a));

            // Find the expr for the format string.  This may not be exactly args[fmt_idx] - the
            // user can mark the actual string literal in case there are casts/conversions applied.

            let mut old_fmt_str_expr = None;
            visit_nodes(&args[fmt_idx] as &Expr, |e: &Expr| {
                info!("  look at {:?} - marked? {} - {:?}", e.id, st.marked(e.id, "fmt_str"), e);
                if st.marked(e.id, "fmt_str") {
                    if old_fmt_str_expr.is_some() {
                        warn!("multiple fmt_str marks inside argument {:?}", args[fmt_idx]);
                        return;
                    }
                    old_fmt_str_expr = Some(P(e.clone()));
                }
            });
            let old_fmt_str_expr = old_fmt_str_expr.unwrap_or_else(|| args[fmt_idx].clone());

            info!("  found fmt str {:?}", old_fmt_str_expr);

            let lit = expect!([old_fmt_str_expr.node] ExprKind::Lit(ref l) => l);
            let s = expect!([lit.node]
                LitKind::Str(s, _) => (&s.as_str() as &str).to_owned(),
                LitKind::ByteStr(ref b) => str::from_utf8(b).unwrap().to_owned());

            let mut new_s = String::with_capacity(s.len());
            let mut casts = HashMap::new();

            let mut idx = 0;
            Parser::new(&s, |piece| match piece {
                Piece::Text(s) => new_s.push_str(s),
                Piece::Conv(c) => {
                    c.push_spec(&mut new_s);
                    c.add_casts(&mut idx, &mut casts);
                },
            }).parse();

            while new_s.ends_with("\0") {
                new_s.pop();
            }


            let new_fmt_str_expr = mk().lit_expr(mk().str_lit(&new_s));

            info!("old fmt str expr: {:?}", old_fmt_str_expr);
            info!("new fmt str expr: {:?}", new_fmt_str_expr);

            let mut macro_tts: Vec<TokenTree> = Vec::new();
            let expr_tt = |e: P<Expr>| TokenTree::Token(e.span, Token::interpolated(
                    Nonterminal::NtExpr(e)));
            macro_tts.push(expr_tt(new_fmt_str_expr));
            for (i, arg) in args[fmt_idx + 1 ..].iter().enumerate() {
                if let Some(cast) = casts.get(&i) {
                    let tt = expr_tt(cast.apply(arg.clone()));
                    macro_tts.push(TokenTree::Token(DUMMY_SP, Token::Comma));
                    macro_tts.push(tt);
                }
            }
            let mac = mk().mac(vec!["format_args"], macro_tts, MacDelimiter::Parenthesis);

            let mut new_args = args[..fmt_idx].to_owned();
            new_args.push(mk().mac_expr(mac));

            mk().id(st.transfer_marks(e.id)).call_expr(func, new_args)
        })
    }
}


#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CastType {
    Int,
    Uint,
    Usize,
    Char,
    Str,
}

impl CastType {
    fn apply(&self, e: P<Expr>) -> P<Expr> {
        match *self {
            CastType::Int => mk().cast_expr(e, mk().ident_ty("i32")),
            CastType::Uint => mk().cast_expr(e, mk().ident_ty("u32")),
            CastType::Usize => mk().cast_expr(e, mk().ident_ty("usize")),
            CastType::Char => {
                // e as u8 as char
                let e = mk().cast_expr(e, mk().ident_ty("u8"));
                mk().cast_expr(e, mk().ident_ty("char"))
            },
            CastType::Str => {
                // CStr::from_ptr(e as *const i8).to_str().unwrap()
                let e = mk().cast_expr(e, mk().ptr_ty(mk().ident_ty("i8")));
                let cs = mk().call_expr(
                    mk().path_expr(mk().abs_path(vec!["std", "ffi", "CStr", "from_ptr"])),
                    vec![e]);
                let s = mk().method_call_expr(cs, "to_str", Vec::<P<Expr>>::new());
                let call = mk().method_call_expr(s, "unwrap", Vec::<P<Expr>>::new());
                let b = mk().unsafe_().block(vec![mk().expr_stmt(call)]);
                mk().block_expr(b)
            },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ConvType {
    Int,
    Uint,
    /// Hexadecimal uint, maybe capitalized.
    Hex(bool),
    Char,
    Str,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Amount {
    Number(usize),
    NextArg,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Conv {
    ty: ConvType,
    width: Option<Amount>,
    prec: Option<Amount>,
}

impl Conv {
    fn new() -> Conv {
        Conv {
            ty: ConvType::Int,
            width: None,
            prec: None,
        }
    }

    fn add_casts(&self, idx: &mut usize, casts: &mut HashMap<usize, CastType>) {
        if self.width == Some(Amount::NextArg) {
            casts.insert(*idx, CastType::Usize);
            *idx += 1;
        }
        if self.prec == Some(Amount::NextArg) {
            casts.insert(*idx, CastType::Usize);
            *idx += 1;
        }

        let cast = match self.ty {
            ConvType::Int => CastType::Int,
            ConvType::Uint |
            ConvType::Hex(_) => CastType::Uint,
            ConvType::Char => CastType::Char,
            ConvType::Str => CastType::Str,
        };

        casts.insert(*idx, cast);
        *idx += 1;
    }

    fn push_spec(&self, buf: &mut String) {
        buf.push_str("{:");

        if let Some(amt) = self.width {
            match amt {
                Amount::Number(n) => buf.push_str(&n.to_string()),
                Amount::NextArg => buf.push('*'),
            }
        }

        if let Some(amt) = self.prec {
            buf.push('.');
            match amt {
                Amount::Number(n) => buf.push_str(&n.to_string()),
                Amount::NextArg => buf.push('*'),
            }
        }

        match self.ty {
            ConvType::Hex(false) => buf.push('x'),
            ConvType::Hex(true) => buf.push('X'),
            _ => {},
        }

        buf.push('}');
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Piece<'a> {
    Text(&'a str),
    Conv(Box<Conv>),
}

struct Parser<'a, F: FnMut(Piece)> {
    s: &'a str,
    sb: &'a [u8],
    pos: usize,
    callback: F,
}

impl<'a, F: FnMut(Piece)> Parser<'a, F> {
    fn new(s: &'a str, callback: F) -> Parser<'a, F> {
        Parser {
            s: s,
            sb: s.as_bytes(),
            pos: 0,
            callback: callback,
        }
    }

    fn peek(&self) -> u8 {
        self.sb[self.pos]
    }
    fn skip(&mut self) {
        self.pos += 1;
    }

    /// Try to advance to the next conversion specifier.  Return `true` if a conversion was found.
    fn next_conv(&mut self) -> bool {
        if let Some(conv_offset) = self.s[self.pos..].find('%') {
            if conv_offset > 0 {
                let conv_pos = self.pos + conv_offset;
                (self.callback)(Piece::Text(&self.s[self.pos..conv_pos]));
                self.pos = conv_pos;
            }
            true
        } else {
            false
        }
    }

    fn parse(&mut self) {
        while self.next_conv() {
            self.skip();
            let mut conv = Conv::new();

            if self.peek() == b'%' {
                self.skip();
                (self.callback)(Piece::Text("%"));
                continue;
            }

            if b'1' <= self.peek() && self.peek() <= b'9' || self.peek() == b'*'{
                conv.width = Some(self.parse_amount());
            } 
            if self.peek() == b'.' {
                self.skip();
                conv.prec = Some(self.parse_amount());
            }
            conv.ty = self.parse_conv_type();
            (self.callback)(Piece::Conv(Box::new(conv)));
        }

        if self.pos < self.s.len() {
            (self.callback)(Piece::Text(&self.s[self.pos..]));
        }
    }

    fn parse_amount(&mut self) -> Amount {
        if self.peek() == b'*' {
            self.skip();
            return Amount::NextArg;
        }

        let start = self.pos;
        while b'0' <= self.peek() && self.peek() <= b'9' {
            self.skip();
        }
        let end = self.pos;

        Amount::Number(usize::from_str(&self.s[start..end]).unwrap())
    }

    fn parse_conv_type(&mut self) -> ConvType {
        let c = self.peek() as char;
        self.skip();

        match c {
            'd' => ConvType::Int,
            'u' => ConvType::Uint,
            'x' => ConvType::Hex(false),
            'X' => ConvType::Hex(true),
            'c' => ConvType::Char,
            's' => ConvType::Str,
            _ => panic!("unrecognized conversion spec `{}`", c),
        }
    }
}


pub fn register_commands(reg: &mut Registry) {
    use super::mk;

    reg.register("convert_format_args", |_args| mk(ConvertFormatArgs));
}
