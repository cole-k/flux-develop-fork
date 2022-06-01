use std::{hint::unreachable_unchecked, iter};

use itertools::Itertools;

use rustc_hash::FxHashMap;

use liquid_rust_common::{index::IndexVec, iter::IterExt};
use liquid_rust_middle::{
    rustc::mir::{Field, Place, PlaceElem},
    ty::{
        subst::FVarSubst, AdtDef, BaseTy, Expr, Index, Loc, Path, RefKind, Ty, TyKind, VariantIdx,
    },
};

use crate::{param_infer, refine_tree::RefineCtxt};

#[derive(Clone, Default, Eq, PartialEq)]
pub struct PathsTree {
    map: FxHashMap<Loc, Node>,
}

pub enum LookupResult<'a> {
    Ptr(Path, &'a mut Ty),
    Ref(RefKind, Ty),
}

impl LookupResult<'_> {
    pub fn ty(&self) -> Ty {
        match self {
            LookupResult::Ptr(_, ty) => (*ty).clone(),
            LookupResult::Ref(_, ty) => ty.clone(),
        }
    }
}

impl PathsTree {
    pub fn lookup_place<F, R>(&mut self, rcx: &mut RefineCtxt, place: &Place, f: F) -> R
    where
        F: for<'a> FnOnce(&mut RefineCtxt, LookupResult<'a>) -> R,
    {
        self.lookup_place_iter(rcx, Loc::Local(place.local), &mut place.projection.iter(), f)
    }

    pub fn get(&self, path: &Path) -> Option<&Ty> {
        let mut node = self.map.get(&path.loc)?;
        for f in path.projection() {
            match node {
                Node::Ty(_) => return None,
                Node::Adt(.., fields) => node = &fields[*f],
            }
        }
        match node {
            Node::Ty(ty) => Some(ty),
            Node::Adt(..) => None,
        }
    }

    pub fn get_mut(&mut self, path: &Path) -> Option<&mut Ty> {
        let mut node = self.map.get_mut(&path.loc)?;
        for f in path.projection() {
            match node {
                Node::Ty(_) => return None,
                Node::Adt(.., fields) => node = &mut fields[*f],
            }
        }
        match node {
            Node::Ty(ty) => Some(ty),
            Node::Adt(..) => None,
        }
    }

    pub fn insert(&mut self, loc: Loc, ty: Ty) {
        self.map.insert(loc, Node::Ty(ty));
    }

    pub fn remove(&mut self, loc: Loc) {
        self.map.remove(&loc);
    }

    pub fn locs(&self) -> impl Iterator<Item = Loc> + '_ {
        self.map.keys().copied()
    }

    pub fn contains_loc(&self, loc: Loc) -> bool {
        self.map.contains_key(&loc)
    }

    pub fn iter(&self) -> impl Iterator<Item = (Path, &Ty)> {
        self.map
            .iter()
            .flat_map(|(loc, node)| PathsIter::new(*loc, node))
    }

    pub fn paths(&self) -> impl Iterator<Item = Path> + '_ {
        self.iter().map(|(path, _)| path)
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Path, &mut Ty)> {
        self.map
            .iter_mut()
            .flat_map(|(loc, node)| PathsIterMut::new(*loc, node))
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Ty> {
        self.iter_mut().map(|(_, ty)| ty)
    }

    pub fn subst(self, subst: &FVarSubst) -> Self {
        let map = self
            .map
            .into_iter()
            .map(|(loc, mut node)| {
                node.subst_mut(subst);
                (subst.subst_loc(loc), node)
            })
            .collect();
        PathsTree { map }
    }

    pub fn unfold_with(&mut self, other: &mut PathsTree) {
        for (loc, node1) in &mut self.map {
            if let Some(node2) = other.map.get_mut(loc) {
                node1.unfold_with(node2);
            }
        }
    }

    pub fn fold_unfold_with(&mut self, rcx: &mut RefineCtxt, other: &PathsTree) {
        for (loc, node1) in &mut self.map {
            if let Some(node2) = other.map.get(loc) {
                node1.fold_unfold_with(rcx, node2);
            }
        }
    }

    fn lookup_place_iter<R, F>(
        &mut self,
        rcx: &mut RefineCtxt,
        path: impl Into<Path>,
        place_proj: &mut std::slice::Iter<PlaceElem>,
        f: F,
    ) -> R
    where
        F: for<'a> FnOnce(&mut RefineCtxt, LookupResult<'a>) -> R,
    {
        let mut path = path.into();
        loop {
            let loc = path.loc;
            let mut proj = vec![];

            let mut node = self.map.get_mut(&loc).unwrap();
            for elem in path
                .projection()
                .iter()
                .map(|f| FieldOrDowncast::Field(*f))
                .chain(place_proj.map_take_while(|p| FieldOrDowncast::from_place_elem(p)))
            {
                match elem {
                    FieldOrDowncast::Field(f) => {
                        proj.push(f);
                        node = node.proj(f);
                    }
                    FieldOrDowncast::Downcast(variant_idx) => {
                        node.unfold(variant_idx);
                    }
                }
            }

            match place_proj.next() {
                Some(PlaceElem::Deref) => {
                    let ty = node.fold(rcx);
                    match ty.kind() {
                        TyKind::Ptr(ptr_path) => path = ptr_path.expect_path(),
                        TyKind::Ref(mode, ty) => {
                            let ty = ty.clone();
                            let mode = *mode;
                            let result = self.place_proj_ty(rcx, mode, &ty, place_proj);
                            return f(rcx, result);
                        }
                        _ => unreachable!("type cannot be dereferenced `{ty:?}`"),
                    }
                }
                Some(elem) => unreachable!("expected deref, found `{elem:?}`"),
                None => {
                    let ty = node.fold(rcx);
                    return f(rcx, LookupResult::Ptr(Path::new(loc, proj), ty));
                }
            }
        }
    }

    fn place_proj_ty(
        &mut self,
        rcx: &mut RefineCtxt,
        mut mode: RefKind,
        ty: &Ty,
        proj: &mut std::slice::Iter<PlaceElem>,
    ) -> LookupResult {
        let mut ty = ty.clone();
        while let Some(elem) = proj.next() {
            match (elem, ty.kind()) {
                (PlaceElem::Deref, TyKind::Ref(mode2, ty2)) => {
                    mode = mode.min(*mode2);
                    ty = ty2.clone();
                }
                (PlaceElem::Deref, TyKind::Ptr(path)) => {
                    return self.lookup_place_iter(rcx, path.expect_path(), proj, |_, lookup| {
                        match lookup {
                            LookupResult::Ptr(_, ty) => LookupResult::Ref(mode, ty.clone()),
                            LookupResult::Ref(mode2, ty2) => {
                                LookupResult::Ref(mode.min(mode2), ty2)
                            }
                        }
                    });
                }
                (PlaceElem::Field(f), _) => {
                    let (_, fields) = ty.unfold(VariantIdx::from_u32(0)).unwrap();
                    ty = fields[*f].clone();
                }
                _ => unreachable!(),
            }
        }
        LookupResult::Ref(mode, ty)
    }
}

enum FieldOrDowncast {
    Field(Field),
    Downcast(VariantIdx),
}

impl FieldOrDowncast {
    fn from_place_elem(elem: &PlaceElem) -> Option<FieldOrDowncast> {
        match elem {
            PlaceElem::Field(f) => Some(FieldOrDowncast::Field(*f)),
            PlaceElem::Downcast(variant_idx) => Some(FieldOrDowncast::Downcast(*variant_idx)),
            _ => None,
        }
    }
}

impl<'a> std::ops::Index<&'a Path> for PathsTree {
    type Output = Ty;

    fn index(&self, path: &'a Path) -> &Self::Output {
        match self.get(path) {
            Some(ty) => ty,
            None => panic!("no entry found for path `{:?}`", path),
        }
    }
}

impl std::ops::Index<Path> for PathsTree {
    type Output = Ty;

    fn index(&self, path: Path) -> &Self::Output {
        match self.get(&path) {
            Some(ty) => ty,
            None => panic!("no entry found for path `{:?}`", path),
        }
    }
}

impl<'a> std::ops::IndexMut<&'a Path> for PathsTree {
    fn index_mut(&mut self, path: &'a Path) -> &mut Self::Output {
        match self.get_mut(path) {
            Some(ty) => ty,
            None => panic!("no entry found for path `{:?}`", path),
        }
    }
}

impl std::ops::IndexMut<Path> for PathsTree {
    fn index_mut(&mut self, path: Path) -> &mut Self::Output {
        match self.get_mut(&path) {
            Some(ty) => ty,
            None => panic!("no entry found for path `{:?}`", path),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
enum Node {
    Ty(Ty),
    Adt(AdtDef, VariantIdx, IndexVec<Field, Node>),
}

impl Node {
    fn unfold_with(&mut self, other: &mut Node) {
        let (fields1, fields2) = match (&mut *self, &mut *other) {
            (Node::Ty(_), Node::Ty(_)) => return,
            (Node::Ty(_), Node::Adt(_, variant_idx, fields2)) => {
                (self.unfold(*variant_idx), fields2)
            }
            (Node::Adt(_, variant_idx, fields1), Node::Ty(_)) => {
                (fields1, other.unfold(*variant_idx))
            }
            (Node::Adt(_, variant_idx1, fields1), Node::Adt(_, variant_idx2, fields2)) => {
                debug_assert_eq!(variant_idx1, variant_idx2);
                (fields1, fields2)
            }
        };
        debug_assert_eq!(fields1.len(), fields2.len());
        for (field1, field2) in fields1.iter_mut().zip(fields2.iter_mut()) {
            field1.unfold_with(field2);
        }
    }

    fn fold_unfold_with(&mut self, rcx: &mut RefineCtxt, other: &Node) {
        let (fields1, fields2) = match (&mut *self, other) {
            (Node::Ty(_), Node::Ty(_)) => return,
            (Node::Adt(..), Node::Ty(_)) => {
                self.fold(rcx);
                return;
            }
            (Node::Ty(_), Node::Adt(_, variant_idx, fields2)) => {
                (self.unfold(*variant_idx), fields2)
            }
            (Node::Adt(_, variant_idx1, fields1), Node::Adt(_, variant_idx2, fields2)) => {
                debug_assert_eq!(variant_idx1, variant_idx2);
                (fields1, fields2)
            }
        };
        debug_assert_eq!(fields1.len(), fields2.len());
        for (field1, field2) in fields1.iter_mut().zip(fields2) {
            field1.fold_unfold_with(rcx, field2);
        }
    }

    fn unfold(&mut self, variant_idx: VariantIdx) -> &mut IndexVec<Field, Node> {
        if let Node::Ty(ty) = self {
            let (adt_def, fields) = ty.unfold(variant_idx).unwrap();
            *self = Node::Adt(adt_def, variant_idx, fields.into_iter().map(Node::Ty).collect());
        }
        match self {
            Node::Ty(_) => unreachable!(),
            Node::Adt(_, node_variant_idx, fields) => {
                debug_assert_eq!(variant_idx, *node_variant_idx);
                fields
            }
        }
    }

    fn proj(&mut self, field: Field) -> &mut Node {
        match self {
            Node::Ty(_) => &mut self.unfold(VariantIdx::from_u32(0))[field],
            Node::Adt(_, _, fields) => &mut fields[field],
        }
    }

    fn fold(&mut self, rcx: &mut RefineCtxt) -> &mut Ty {
        match self {
            Node::Ty(ty) => ty,
            Node::Adt(adt_def, variant_idx, fields) => {
                let fields = fields.iter_mut().map(|n| n.fold(rcx).clone()).collect_vec();
                let indices = fold(rcx, adt_def, &fields[..], *variant_idx);
                let adt = BaseTy::adt(adt_def.clone(), vec![]);
                let ty = Ty::indexed(adt, indices);
                *self = Node::Ty(ty);
                if let Node::Ty(ty) = self {
                    ty
                } else {
                    unsafe { unreachable_unchecked() }
                }
            }
        }
    }

    fn subst_mut(&mut self, subst: &FVarSubst) {
        match self {
            Node::Ty(ty) => *ty = subst.apply(ty),
            Node::Adt(.., fields) => {
                for field in fields.iter_mut() {
                    field.subst_mut(subst);
                }
            }
        }
    }
}

type ParamInst = FxHashMap<usize, Expr>;

fn fold(rcx: &mut RefineCtxt, adt_def: &AdtDef, tys: &[Ty], variant_idx: VariantIdx) -> Vec<Index> {
    let mut params = FxHashMap::default();
    let variant_sig = adt_def.variant_sig(variant_idx);
    for (ty1, ty2) in iter::zip(tys, variant_sig.skip_binders().args()) {
        ty_infer_folding(rcx, &mut params, ty1, ty2);
    }
    adt_def
        .sorts()
        .iter()
        .enumerate()
        .map(|(idx, _)| params.remove(&idx).unwrap().into())
        .collect()
}

fn ty_infer_folding(rcx: &mut RefineCtxt, params: &mut ParamInst, ty1: &Ty, ty2: &Ty) {
    match (ty1.kind(), ty2.kind()) {
        (TyKind::Indexed(bty1, indices1), TyKind::Indexed(bty2, indices2)) => {
            bty_infer_folding(rcx, params, bty1, bty2);
            for (idx1, idx2) in iter::zip(indices1, indices2) {
                param_infer::infer_from_exprs(params, &idx1.expr, &idx2.expr);
            }
        }
        (TyKind::Ptr(_), TyKind::Ptr(_)) => todo!(),
        (TyKind::Ref(RefKind::Shr, ty1), TyKind::Ref(RefKind::Shr, ty2)) => {
            ty_infer_folding(rcx, params, ty1, ty2);
        }
        _ => {}
    }
}

fn bty_infer_folding(rcx: &mut RefineCtxt, params: &mut ParamInst, bty1: &BaseTy, bty2: &BaseTy) {
    if let (BaseTy::Adt(def1, substs1), BaseTy::Adt(def2, substs2)) = (bty1, bty2) {
        debug_assert_eq!(def1.def_id(), def2.def_id());
        debug_assert_eq!(substs1.len(), substs2.len());
        for (ty1, ty2) in iter::zip(substs1, substs2) {
            ty_infer_folding(rcx, params, ty1, ty2);
        }
    }
}

enum PathsIter<'a> {
    Adt {
        stack: Vec<std::iter::Enumerate<std::slice::Iter<'a, Node>>>,
        loc: Loc,
        projection: Vec<Field>,
    },
    Ty(Option<(Loc, &'a Ty)>),
}

impl<'a> PathsIter<'a> {
    fn new(loc: Loc, root: &'a Node) -> Self {
        match root {
            Node::Ty(ty) => PathsIter::Ty(Some((loc, ty))),
            Node::Adt(.., fields) => {
                PathsIter::Adt { loc, projection: vec![], stack: vec![fields.iter().enumerate()] }
            }
        }
    }
}

impl<'a> Iterator for PathsIter<'a> {
    type Item = (Path, &'a Ty);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            PathsIter::Adt { stack, loc, projection } => {
                while let Some(top) = stack.last_mut() {
                    if let Some((i, node)) = top.next() {
                        match node {
                            Node::Adt(.., fields) => {
                                projection.push(Field::from(i));
                                stack.push(fields.iter().enumerate());
                            }
                            Node::Ty(ty) => {
                                projection.push(Field::from(i));
                                let path = Path::new(*loc, projection.as_slice());
                                projection.pop();
                                return Some((path, ty));
                            }
                        }
                    } else {
                        projection.pop();
                        stack.pop();
                    }
                }
                None
            }
            PathsIter::Ty(item) => item.take().map(|(loc, ty)| (Path::new(loc, vec![]), ty)),
        }
    }
}

enum PathsIterMut<'a> {
    Adt {
        stack: Vec<std::iter::Enumerate<std::slice::IterMut<'a, Node>>>,
        loc: Loc,
        projection: Vec<Field>,
    },
    Ty(Option<(Loc, &'a mut Ty)>),
}

impl<'a> PathsIterMut<'a> {
    fn new(loc: Loc, root: &'a mut Node) -> Self {
        match root {
            Node::Ty(ty) => PathsIterMut::Ty(Some((loc, ty))),
            Node::Adt(.., fields) => {
                PathsIterMut::Adt {
                    loc,
                    projection: vec![],
                    stack: vec![fields.iter_mut().enumerate()],
                }
            }
        }
    }
}

impl<'a> Iterator for PathsIterMut<'a> {
    type Item = (Path, &'a mut Ty);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            PathsIterMut::Adt { stack, loc, projection } => {
                while let Some(top) = stack.last_mut() {
                    if let Some((i, node)) = top.next() {
                        match node {
                            Node::Adt(.., fields) => {
                                projection.push(Field::from(i));
                                stack.push(fields.iter_mut().enumerate());
                            }
                            Node::Ty(ty) => {
                                projection.push(Field::from(i));
                                let path = Path::new(*loc, projection.as_slice());
                                projection.pop();
                                return Some((path, ty));
                            }
                        }
                    } else {
                        projection.pop();
                        stack.pop();
                    }
                }
                None
            }
            PathsIterMut::Ty(item) => item.take().map(|(loc, ty)| (Path::new(loc, vec![]), ty)),
        }
    }
}
