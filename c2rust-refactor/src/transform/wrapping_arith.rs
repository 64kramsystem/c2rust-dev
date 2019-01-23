use syntax::ast::Crate;

use crate::api::*;
use crate::command::{CommandState, Registry};
use crate::driver;
use crate::transform::Transform;


/// # `wrapping_arith_to_normal` Command
/// 
/// Usage: `wrapping_arith_to_normal`
/// 
/// Replace all uses of wrapping arithmetic methods with ordinary arithmetic
/// operators.  For example, replace `x.wrapping_add(y)` with `x + y`.
pub struct WrappingToNormal;

impl Transform for WrappingToNormal {
    fn transform(&self, krate: Crate, st: &CommandState, cx: &driver::Ctxt) -> Crate {
        let krate = replace_expr(st, cx, krate,
                                 "__x.wrapping_add(__y)",
                                 "__x + __y");
        let krate = replace_expr(st, cx, krate,
                                 "__x.wrapping_sub(__y)",
                                 "__x - __y");
        let krate = replace_expr(st, cx, krate,
                                 "__x.wrapping_mul(__y)",
                                 "__x * __y");
        let krate = replace_expr(st, cx, krate,
                                 "__x.wrapping_div(__y)",
                                 "__x / __y");
        let krate = replace_expr(st, cx, krate,
                                 "__x.wrapping_rem(__y)",
                                 "__x % __y");
        let krate = replace_expr(st, cx, krate,
                                 "__x.wrapping_neg()",
                                 "-__x");
        let krate = replace_expr(st, cx, krate,
                                 "__x.wrapping_shl(__y)",
                                 "__x << __y");
        let krate = replace_expr(st, cx, krate,
                                 "__x.wrapping_shr(__y)",
                                 "__x >> __y");
        let krate = replace_expr(st, cx, krate,
                                 "__x.wrapping_abs()",
                                 "__x.abs()");
        krate
    }
}


pub fn register_commands(reg: &mut Registry) {
    use super::mk;

    reg.register("wrapping_arith_to_normal", |_args| mk(WrappingToNormal));
}
