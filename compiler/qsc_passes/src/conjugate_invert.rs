// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod tests;

use std::{collections::HashSet, mem::take};

use miette::Diagnostic;
use qsc_data_structures::span::Span;
use qsc_frontend::compile::CompileUnit;
use qsc_hir::{
    assigner::Assigner,
    global::Table,
    hir::{
        Block, CallableDecl, Expr, ExprKind, Ident, Mutability, NodeId, Pat, PatKind, Res, Stmt,
        StmtKind, Ty,
    },
    mut_visit::{self, MutVisitor},
    visit::{self, Visitor},
};
use thiserror::Error;

use crate::{
    invert_block::adj_invert_block,
    spec_gen::adj_gen::{self, AdjDistrib},
};

#[derive(Clone, Debug, Diagnostic, Error)]
pub enum Error {
    #[diagnostic(transparent)]
    #[error(transparent)]
    AdjGen(adj_gen::Error),

    #[error("variable cannot be assigned in apply-block since it is used in within-block")]
    #[diagnostic(help("updating mutable variables in the apply-block that are used in the within-block can violate logic reversibility"))]
    ApplyAssign(#[label] Span),

    #[error("return expressions are not allowed in apply-blocks")]
    ReturnForbidden(#[label] Span),
}

/// Generates adjoint inverted blocks for within-blocks across all conjugate expressions,
/// eliminating the conjugate expression from the compilation unit.
pub fn invert_conjugate_exprs(core: &Table, unit: &mut CompileUnit) -> Vec<Error> {
    let mut pass = ConjugateElim {
        core,
        assigner: &mut unit.assigner,
        errors: Vec::new(),
    };
    pass.visit_package(&mut unit.package);
    pass.errors
}

pub fn invert_conjugate_exprs_for_callable(
    core: &Table,
    assigner: &mut Assigner,
    decl: &mut CallableDecl,
) -> Vec<Error> {
    let mut pass = ConjugateElim {
        core,
        assigner,
        errors: Vec::new(),
    };
    pass.visit_callable_decl(decl);
    pass.errors
}

pub fn invert_conjugate_exprs_for_stmt(
    core: &Table,
    assigner: &mut Assigner,
    stmt: &mut Stmt,
) -> Vec<Error> {
    let mut pass = ConjugateElim {
        core,
        assigner,
        errors: Vec::new(),
    };
    pass.visit_stmt(stmt);
    pass.errors
}

struct ConjugateElim<'a> {
    core: &'a Table,
    assigner: &'a mut Assigner,
    errors: Vec<Error>,
}

impl<'a> MutVisitor for ConjugateElim<'a> {
    fn visit_expr(&mut self, expr: &mut Expr) {
        match take(&mut expr.kind) {
            ExprKind::Conjugate(within, apply) => {
                let mut usage = Usage {
                    used: HashSet::new(),
                };
                usage.visit_block(&within);
                let mut assign_check = AssignmentCheck {
                    used: usage.used,
                    errors: Vec::new(),
                };
                assign_check.visit_block(&apply);
                self.errors.extend(assign_check.errors);

                let mut return_check = ReturnCheck { errors: Vec::new() };
                return_check.visit_block(&apply);
                self.errors.extend(return_check.errors);

                let mut adj_within = within.clone();
                if let Err(invert_errors) =
                    adj_invert_block(self.core, self.assigner, &mut adj_within)
                {
                    self.errors.extend(
                        invert_errors
                            .into_iter()
                            .map(adj_gen::Error::LogicSep)
                            .map(Error::AdjGen),
                    );
                    return;
                }
                let mut distrib = AdjDistrib { errors: Vec::new() };
                distrib.visit_block(&mut adj_within);
                self.errors
                    .extend(distrib.errors.into_iter().map(Error::AdjGen));

                let (bind_id, apply_as_bind) =
                    block_as_binding(apply, expr.ty.clone(), self.assigner);

                let new_block = Block {
                    id: NodeId::default(),
                    span: Span::default(),
                    ty: expr.ty.clone(),
                    stmts: vec![
                        block_as_stmt(within),
                        apply_as_bind,
                        block_as_stmt(adj_within),
                        Stmt {
                            id: NodeId::default(),
                            span: Span::default(),
                            kind: StmtKind::Expr(Expr {
                                id: NodeId::default(),
                                span: Span::default(),
                                ty: expr.ty.clone(),
                                kind: ExprKind::Var(Res::Local(bind_id)),
                            }),
                        },
                    ],
                };
                *expr = block_as_expr(new_block, expr.ty.clone());
            }
            kind => expr.kind = kind,
        }

        mut_visit::walk_expr(self, expr);
    }
}

fn block_as_expr(block: Block, ty: Ty) -> Expr {
    Expr {
        id: NodeId::default(),
        span: Span::default(),
        ty,
        kind: ExprKind::Block(block),
    }
}

fn block_as_stmt(block: Block) -> Stmt {
    Stmt {
        id: NodeId::default(),
        span: Span::default(),
        kind: StmtKind::Expr(block_as_expr(block, Ty::UNIT)),
    }
}

fn block_as_binding(block: Block, ty: Ty, assigner: &mut Assigner) -> (NodeId, Stmt) {
    let bind_id = assigner.next_id();
    (
        bind_id,
        Stmt {
            id: assigner.next_id(),
            span: Span::default(),
            kind: StmtKind::Local(
                Mutability::Immutable,
                Pat {
                    id: assigner.next_id(),
                    span: Span::default(),
                    ty: ty.clone(),
                    kind: PatKind::Bind(Ident {
                        id: bind_id,
                        span: Span::default(),
                        name: "apply_res".into(),
                    }),
                },
                block_as_expr(block, ty),
            ),
        },
    )
}

struct Usage {
    used: HashSet<NodeId>,
}

impl<'a> Visitor<'a> for Usage {
    fn visit_expr(&mut self, expr: &'a Expr) {
        match &expr.kind {
            ExprKind::Var(Res::Local(id)) => {
                self.used.insert(*id);
            }
            _ => visit::walk_expr(self, expr),
        }
    }
}

struct AssignmentCheck {
    used: HashSet<NodeId>,
    errors: Vec<Error>,
}

impl<'a> Visitor<'a> for AssignmentCheck {
    fn visit_expr(&mut self, expr: &'a Expr) {
        match &expr.kind {
            ExprKind::Assign(lhs, rhs) => {
                self.visit_expr(rhs);
                self.check_assign(lhs);
            }
            _ => visit::walk_expr(self, expr),
        }
    }
}

impl AssignmentCheck {
    fn check_assign(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Hole => {}
            ExprKind::Var(Res::Local(id)) => {
                if self.used.contains(id) {
                    self.errors.push(Error::ApplyAssign(expr.span));
                }
            }
            ExprKind::Tuple(var_tup) => {
                for expr in var_tup {
                    self.check_assign(expr);
                }
            }
            _ => panic!("unexpected expr type in assignment"),
        }
    }
}

struct ReturnCheck {
    errors: Vec<Error>,
}

impl<'a> Visitor<'a> for ReturnCheck {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if matches!(&expr.kind, ExprKind::Return(..)) {
            self.errors.push(Error::ReturnForbidden(expr.span));
        }
    }
}