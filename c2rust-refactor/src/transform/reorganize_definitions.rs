use smallvec::SmallVec;
use std::collections::{HashMap, HashSet};
use syntax::ast::*;
use syntax::attr;
use syntax::ptr::P;
use syntax::symbol::keywords;
use syntax::visit::{self, Visitor};
use crate::transform::Transform;
use indexmap::IndexSet;

use crate::api::*;
use crate::ast_manip::AstEquiv;
use crate::command::{CommandState, Registry};
use crate::driver::{self, Phase};

/// # `reoganize_definitions` Command
/// 
/// Usage: `reorganize_definitions`
/// 
/// This Command should be ran as soon as the user is done transpiling
/// the file or code base with the `--reorganize-definitions` flag in `c2rust-transpiler`.
/// 
/// What this pass aims to achieve, is depollute a Crate from having the same declarations
/// in every module. This will make a Crate more idiomatic by having imports as opposed to forward
/// declarations everywhere. Look at `c2rust-refactor/tests/reorganize_definitions` for an example.
pub struct ReorganizeDefinitions;

/// Holds the information of the current `Crate`, which includes a `HashMap` to look up Items
/// quickly, as well as other members that hold important information.
pub struct CrateInfo<'st> {
    /// Mapping for fast item lookup, stops the need of having to search the entire Crate.
    item_map: HashMap<NodeId, Item>,

    /// Maps a header declaration item id to a new destination module id.
    item_to_dest_module: HashMap<NodeId, NodeId>,

    /// This is used for mapping modules that need to be created to a new node id
    /// e.g.: "stdlib" -> `NodeId`
    new_modules: HashMap<Ident, NodeId>,

    /// Set of module `NodeId`'s where "old" module items will be sent to
    possible_destination_modules: HashSet<NodeId>,

    /// Old path NodeId -> (New Path, Destination module id)
    path_mapping: HashMap<NodeId, (Path, NodeId)>,

    /// Helper, to expedite the look up of paths
    path_ids: HashSet<NodeId>,
    path_info: PathInfo,

    st: &'st CommandState,
}

#[derive(Default)]
struct PathInfo {
    // old_module_ident -> set_of_segments
    old: HashMap<Ident, HashSet<Ident>>,
    // new_module_ident -> set_of_segments
    new: HashMap<Ident, HashSet<Ident>>,
}

impl<'st> CrateInfo<'st> {
    fn new(st: &'st CommandState) -> Self {
        let mut new_modules = HashMap::new();
        new_modules.insert(Ident::from_str("stdlib"), st.next_node_id());
        CrateInfo {
            new_modules,
            st,
            item_map:                     HashMap::new(),
            item_to_dest_module:          HashMap::new(),
            possible_destination_modules: HashSet::new(),
            path_mapping:                 HashMap::new(),
            path_ids:                     HashSet::new(),
            path_info:                    Default::default(),
        }
    }

    /// Iterates through the Crate, to find any potentential "destination modules",
    /// if one is found it is inserted into `possible_destination_modules`.
    fn find_destination_modules(&mut self, krate: &Crate) {
        // visit all the modules, and find potential destination module canidates
        // also build up the item map here
        visit_nodes(krate, |i: &Item| {
            match i.node {
                ItemKind::Mod(_) => {
                    if !has_source_header(&i.attrs) && !is_std(&i.attrs) {
                        self.possible_destination_modules.insert(i.id);
                    }
                },
                ItemKind::Use(_) => {
                    self.path_ids.insert(i.id);
                },
                _ => {}
            }
            self.item_map.insert(i.id, i.clone());
        });
    }

    /// In this function we try to match an `Item` to a destination module,
    /// once we have a match, the NodeId and the Ident of the module is returned.
    fn find_destination_id(
        &mut self,
        item_to_process: &NodeId,
        old_module: &Item, // Parent of `item_to_process`
    ) -> (NodeId, Ident) {
        if is_std(&old_module.attrs) {
            let node_id = *self.new_modules.get(&Ident::from_str("stdlib")).unwrap();
            let ident = Ident::from_str("stdlib");
            return (node_id, ident);
        }

        // iterate through the set of possible destinations and try to find a possible match
        for dest_module_id in &self.possible_destination_modules {
            if let Some(dest_module) = self.item_map.get(dest_module_id) {
                let dest_module_ident = dest_module.ident;

                // TODO: This is a simple naive heuristic,
                // and should be improved upon.
                if old_module
                    .ident
                    .as_str()
                    .contains(&*dest_module_ident.as_str())
                {
                    let node_id = dest_module.id;
                    let ident = dest_module_ident;
                    return (node_id, ident);
                }
            }
        }

        assert!(!self.item_to_dest_module.contains_key(item_to_process));
        let new_modules = &mut self.new_modules;
        let state = &self.st;
        let node_id = *new_modules
            .entry(old_module.ident)
            .or_insert_with(|| state.next_node_id());
        let ident = old_module.ident;
        (node_id, ident)
    }

    /// Inserts `Item`'s into a previously existing `Mod`
    fn insert_items_into_dest(
        &mut self,
        krate: Crate,
        dest_mod_to_items: &HashMap<NodeId, IndexSet<NodeId>>,
    ) -> Crate {

        #[derive(Default)]
        struct InsertionInfo {
            item_map:       HashMap<NodeId, Item>,
            item_to_parent: HashMap<Ident, Ident>,

            old_items:      HashMap<Ident, P<Item>>,
            foreign_mods:   Vec<P<Item>>,
            use_stmts:      Vec<P<Item>>,
        }

        impl InsertionInfo {
            fn reset(&mut self) {
                self.old_items = HashMap::new();
                self.foreign_mods = Vec::new();
                self.use_stmts = Vec::new();
            }
        }

        let mut insertion_info: InsertionInfo = Default::default();

        visit_nodes(&krate, |i: &Item| {
            insertion_info.item_map.insert(i.id, i.clone());
            match i.node {
                ItemKind::Mod(ref m) => {
                    for item in &m.items {
                        // Structs can be forward declared, but do not need
                        // the `no_mangle` attribute.
                        let is_struct = match item.node {
                            ItemKind::Struct(..) => true,
                            _ => false
                        };
                        if attr::contains_name(&item.attrs, "no_mangle") || is_struct {
                            insertion_info.item_to_parent.insert(item.ident, i.ident.clone());
                        }
                    }
                },
                _ => {}
            }
        });

        // This is where items get inserted into the corresponding
        // "destination module"
        let krate = fold_nodes(krate, |pi: P<Item>| {
            if has_source_header(&pi.attrs) || is_std(&pi.attrs) {
                return SmallVec::new();
            }

            let pi = pi.map(|mut i| {
                i.node = match i.node {
                    ItemKind::Mod(mut m) => {
                        let new_item_ids = match dest_mod_to_items.get(&i.id) {
                            Some(x) => x,
                            None => {
                                return Item {
                                    node: ItemKind::Mod(m),
                                    ..i
                                };
                            },
                        };

                        for item in &m.items {
                            if !item.ident.as_str().is_empty() {
                                insertion_info.old_items.insert(item.ident, item.clone());
                            }

                            match item.node {
                                ItemKind::ForeignMod(_) => {insertion_info.foreign_mods.push(item.clone())},
                                ItemKind::Use(_) => {insertion_info.use_stmts.push(item.clone())},
                                _ => {}
                            }
                        }

                        for new_item_id in new_item_ids {
                            let mut new_item = insertion_info.item_map.remove(new_item_id)
                                .unwrap_or_else(|| panic!("There should be a node here: {}", new_item_id));
                            // Since `Use` statements do not have `Ident`'s,
                            // it is necessary to iterate through all the module's use statements
                            // and compare.
                            let mut found = false;
                            let mut is_use_stmt = false;
                            match new_item.node {
                                ItemKind::Use(_) => {
                                    is_use_stmt = true;
                                    for use_stmt in &insertion_info.use_stmts {
                                        if compare_items(&use_stmt, &new_item) {
                                            found = true;
                                        }
                                    }
                                },
                                ItemKind::ForeignMod(ref mut fm) => {
                                    fm.items.retain(|fm_item| !insertion_info.old_items.contains_key(&fm_item.ident));
                                },
                                _ => {}
                            }

                            // TODO:
                            // When NLL releases, see if it's possible to put this in the match
                            // above.
                            if !is_use_stmt {
                                for foreign_mod in &insertion_info.foreign_mods {
                                    if compare_items(&foreign_mod, &new_item) {
                                        found = true;
                                    }
                                }

                                if let Some(old_item) = insertion_info.old_items.get(&new_item.ident) {
                                    if compare_items(&old_item, &new_item) {
                                        found = true;
                                    }
                                }
                            }
                            if !found {
                                insertion_info.item_to_parent.insert(new_item.ident, i.ident.clone());
                                m.items.push(P(new_item));
                            }
                        }

                        let old_parent_ident = i.ident.clone();
                        // look for declarations that also have definitions elsewhere in the crate
                        for module_item in &mut m.items {
                            if let ItemKind::ForeignMod(ref mut foreign_mod) = module_item.node {
                                foreign_mod.items.retain(|foreign_item| {
                                    // if there is a definition somewhere else in the crate, put
                                    // the idents in the map
                                    if let Some(new_parent_ident) = insertion_info.item_to_parent.get(&foreign_item.ident) {
                                        self.path_info.old.entry(old_parent_ident).or_insert_with(HashSet::new).insert(foreign_item.ident);
                                        self.path_info.new.entry(*new_parent_ident).or_insert_with(HashSet::new).insert(foreign_item.ident);
                                        return false;
                                    }
                                    true
                                });
                            }
                        }
                        insertion_info.reset();
                        ItemKind::Mod(m)
                    },
                    n => n,
                };
                i
            });
            smallvec![pi]
        });
        krate
    }
}

impl<'ast, 'st> Visitor<'ast> for CrateInfo<'st> {
    // Match the modules, using a mapping like:
    // NodeId -> NodeId
    // The key is the id of the old item to be moved, and the value is the NodeId of the module
    // the item will be moved to.
    fn visit_item(&mut self, old_module: &'ast Item) {
        if has_source_header(&old_module.attrs) {
            match old_module.node {
                ItemKind::Mod(ref m) => {
                    let mut path_info = HashMap::new();
                    for module_item in m.items.iter() {
                        let (dest_module_id, ident) =
                            self.find_destination_id(&module_item.id, &old_module);
                        self.item_to_dest_module
                            .insert(module_item.id, dest_module_id);

                        path_info.insert(old_module.ident, (dest_module_id, ident));
                    }

                    for use_id in self.path_ids.iter() {
                        let item = self.item_map.get(&use_id)
                            .unwrap_or_else(|| panic!("There should be an item here: {:#?}", use_id));
                        let ut = match item.node {
                            ItemKind::Use(ref ut) => ut,
                            _ => unreachable!(),
                        };

                        let (prefix, dest_id) = self.path_mapping.entry(*use_id).or_insert_with(|| {
                            let mut prefix = ut.prefix.clone();

                            // Remove super and self from the paths
                            if prefix.segments.len() > 1 {
                                prefix.segments.retain(|segment| {
                                    segment.ident.name != keywords::Super.name()
                                        && segment.ident.name != keywords::SelfValue.name()
                                });
                            }
                            (prefix, *use_id)
                        });

                        // Check to see if a segment within the path is getting moved.
                        // example_h -> example
                        // DUMMY_NODE_ID -> actual destination module id
                        for segment in &mut prefix.segments {
                            if let Some((dest_module_id, ident)) = path_info.get(&segment.ident) {
                                segment.ident = *ident;
                                *dest_id = *dest_module_id;
                            }
                        }
                    }
                },
                _ => {}
            }
        }
        visit::walk_item(self, old_module);
    }
}


/// This is where a bulk of the duplication removal happens, as well as path clean up.
/// 1. Paths are updated, meaning either removed or changed to match module change.
///      And then reinserted with the new set of prefixes.
/// 2. Removes duplicates from `ForeignMod`'s
/// 3. Duplicate `Item`s are removed
fn deduplicate_krate(krate: Crate, krate_info: &CrateInfo) -> Crate {
    struct DeduplicationInfo<'pi> {
        path_info:        &'pi PathInfo,
        seen_paths:       HashMap<Ident, HashSet<Ident>>,
        new_paths:        HashSet<Ident>,
        seen_item_ids:    HashSet<NodeId>,
        deleted_item_ids: HashSet<NodeId>,
    }
    impl<'pi> DeduplicationInfo<'pi> {
        fn new(path_info: &'pi PathInfo) -> Self {
            DeduplicationInfo {
                path_info,
                seen_paths:       HashMap::new(),
                new_paths:        HashSet::new(),
                seen_item_ids:    HashSet::new(),
                deleted_item_ids: HashSet::new(),
            }
        }

        /// Part of the removal of forward declarations, this updates use statements to correctly use
        /// definitions as opposed to the deleted declarations.
        fn update_paths(&mut self, current_mod_name: &Ident) {
            let mut new_path_info = self.path_info.new.clone();
            for (module_name, set_of_segments) in self.seen_paths.iter_mut() {
                if let Some(segments_to_remove) = self.path_info.old.get(&module_name) {
                    let copy = set_of_segments.clone();
                    let diff = copy.into_iter().filter(|k| !segments_to_remove.contains(k)).collect();
                    *set_of_segments = diff;
                }

                if let Some(segments_to_add) = new_path_info.remove(&module_name) {
                    set_of_segments.extend(segments_to_add.iter());
                }
            }

            for (module_name, segments_to_add) in new_path_info.iter() {
                if current_mod_name != module_name {
                    self.seen_paths.insert(*module_name, segments_to_add.clone());
                }
            }
        }

        fn update_and_insert_paths(&mut self, module: &mut Mod, module_ident: &Ident) -> Vec<P<Item>> {
            // Update paths so the definitions can be used in path segments instead of
            // the declaration
            self.update_paths(module_ident);

            let already_in_use = |path, seen_paths: &HashMap<Ident, HashSet<Ident>>| -> bool {
                seen_paths.values().any(|set_of_segments| set_of_segments.contains(path))
            };

            let mut item_idents = HashSet::new();
            let mut use_stmts = Vec::new();

            for item in &module.items {
                item_idents.insert(item.ident);
                match item.node {
                    ItemKind::Use(_) => {
                        use_stmts.push(item.clone());
                    },
                    _ => {}
                }
            }
            // On occasions where there is a use statement:
            // `use super::{libc, foo};`.
            // This is where a the statement is seperated, and turned into simple
            // statements for every nested segment. The simple statements are
            // inserted if there is no other occurence of that statement within the module already.
            for new_path in &self.new_paths {
                if !item_idents.contains(new_path) && !already_in_use(new_path, &self.seen_paths) {
                    let path = mk().use_item(Path::from_ident(*new_path), None as Option<Ident>);
                    if use_stmts.is_empty() {
                        module.items.push(path.clone());
                    } else {
                        for use_stmt in &use_stmts {
                            if !compare_items(&path, use_stmt) {
                                module.items.push(path.clone());
                            }
                        }
                    }
                }
            }
            // `seen_paths` turns into `use foo_h::{item, item2, item3};`
            // That Path is then pushed into the module
            let mut use_items = Vec::with_capacity(self.seen_paths.len());
            for (mod_name, prefixes) in &mut self.seen_paths {
                let items: Vec<Ident> = prefixes.iter().map(|i| i).cloned().collect();
                let mod_prefix = Path::from_ident(*mod_name);

                // Removes duplicates from the nested use statement
                prefixes.retain(|prefix| !item_idents.contains(&*prefix));

                use_items.push(mk().use_multiple_item(mod_prefix, items));
            }
            // Put the use stmts at the top of the module
            use_items.append(&mut module.items);
            use_items
        }
    }

    let krate = fold_nodes(krate, |pi: P<Item>| {
        let pi = pi.map(|mut i| {
            i.node = match i.node {
                ItemKind::Mod(ref m) => {
                    let mut m = m.clone();

                    let mut ddi = DeduplicationInfo::new(&krate_info.path_info);

                    // This iteration goes through the module items and finds use statements,
                    // and either removes use statements or modifies them to have correct the
                    // module name.
                    ddi.seen_item_ids =
                        m.items.iter().map(|item| item.id).collect::<HashSet<_>>();

                    // TODO: Use a function for `filter_map`
                    m.items = m.items.into_iter().filter_map(|mut module_item| {
                        if let Some((_, dest_module_id)) = krate_info.path_mapping.get(&module_item.id) {
                            if i.id == *dest_module_id {
                                return None;
                            }
                        }

                        if let ItemKind::Use(ref ut) = module_item.node {
                            if let Some((new_path, _)) = krate_info.path_mapping.get(&module_item.id) {
                                let mut ut = ut.clone();
                                ut.prefix = new_path.clone();
                                // In some modules there are multiple nested use statements that may
                                // import differing prefixes, but also duplicate prefixes. So what
                                // happens here is if there is a nested use statement:
                                // 1. insert all the prefixes in a set
                                // 2. If the module name is already in seen_paths, create a union of
                                //    the existing set with the set of prefixes we just created and
                                //    override.
                                //    Else just insert that set into the map.
                                //    [foo_h] -> [module_item, module_item2, module_item3]
                                //  3. delete the nested use statement.
                                match ut.kind {
                                    UseTreeKind::Nested(ref use_trees) => {
                                        let mut segments = HashSet::new();

                                        let mod_prefix = path_to_ident(&ut.prefix);
                                        // This is a check to see if the use statement is:
                                        // `use Super::{module_item, module_item2};`
                                        // If it is we are going to seperate the nested
                                        // statement to be N simple statements, N being the
                                        // number of nested segements.
                                        if mod_prefix.name == keywords::Super.name() ||
                                            mod_prefix.name == keywords::SelfValue.name() {
                                            for (use_tree, _) in use_trees {
                                                ddi.new_paths.insert(path_to_ident(&use_tree.prefix));
                                            }
                                        } else {
                                            for (use_tree, _) in use_trees {
                                                segments.insert(path_to_ident(&use_tree.prefix));
                                            }

                                            ddi.seen_paths.entry(mod_prefix).and_modify(|set_of_segments| {
                                                set_of_segments.extend(segments.clone().into_iter());
                                            }).or_insert_with(|| {
                                                segments
                                            });
                                        }
                                        return None;
                                    },
                                    UseTreeKind::Simple(..) => {
                                        if ut.prefix.segments.len() > 1 {
                                            let mod_name = ut.prefix.segments.first().unwrap();
                                            let segment = ut.prefix.segments.last().unwrap();

                                            let set_of_segments = ddi.seen_paths.entry(mod_name.ident).or_insert_with(HashSet::new);
                                            set_of_segments.insert(segment.ident);
                                            return None;
                                        }
                                    },
                                    _ => {}
                                }
                            }
                        }
                        for item_id in ddi.seen_item_ids.clone().iter() {
                            let item = krate_info.item_map.get(&item_id)
                                .unwrap_or_else(|| panic!("There should be an item here: {:#?}", item_id));
                            if item.id != module_item.id {
                                if let ItemKind::ForeignMod(ref mut foreign_mod) = module_item.node {
                                    if let ItemKind::ForeignMod(ref other_foreign_mod) = item.node {
                                        let other_items: HashMap<Ident, &ForeignItem> = other_foreign_mod.items.iter()
                                            .map(|i| (i.ident, i)).collect::<HashMap<_, _>>();

                                        foreign_mod.items.retain(|foreign_item| {
                                            let mut result = true;
                                            if let Some(other_item) = other_items.get(&foreign_item.ident) {
                                                if compare_foreign_items(&foreign_item, &other_item) && !ddi.deleted_item_ids.contains(&other_item.id) {
                                                    ddi.deleted_item_ids.insert(foreign_item.id);
                                                    result = false;
                                                }
                                            }
                                            result
                                        });
                                    }
                                }

                                match module_item.node {
                                    // Remove empty `ForeignMod`s
                                    ItemKind::ForeignMod(ref foreign_mod) => {
                                        if foreign_mod.items.is_empty() {
                                            return None;
                                        }
                                    },
                                    _ => {
                                        if compare_items(&item, &module_item) && !ddi.deleted_item_ids.contains(&item.id) {
                                            ddi.deleted_item_ids.insert(module_item.id);
                                            return None;
                                        }
                                    }
                                }
                            }
                        }
                        Some(module_item)
                    }).collect();

                    m.items = ddi.update_and_insert_paths(&mut m, &i.ident);
                    ItemKind::Mod(m)
                },
                n => n,
            };
            i
        });
        smallvec![pi]
    });
    krate
}

/// Iterates through `item_to_dest_mod`, and creates a reverse mapping of the HashMap
/// `dest_node_id` -> `Vec<items_to_get_inserted>`
fn create_dest_mod_map(krate_info: &CrateInfo) -> HashMap<NodeId, IndexSet<NodeId>> {
    let mut dest_mod_to_items: HashMap<NodeId, IndexSet<NodeId>> = HashMap::new();
    for (item_id, dest_mod_id) in krate_info.item_to_dest_module.iter() {
        dest_mod_to_items.entry(*dest_mod_id).or_insert_with(IndexSet::new).insert(*item_id);
    }
    dest_mod_to_items
}

/// This function creates a `Mod` with its previous `Item`'s and inserts it into the
/// `Crate`.
fn extend_crate(
    krate: Crate,
    krate_info: &CrateInfo,
    dest_mod_to_items: &HashMap<NodeId, IndexSet<NodeId>>,
) -> Crate {
    let mut krate = krate;
    // inverse new_modules, so we can look up the ident by id
    let inverse_map = krate_info
        .new_modules
        .iter()
        .map(|(ident, id)| (id, ident.clone()))
        .collect::<HashMap<_, _>>();

    // insert the "new modules" into the crate
    for (dest_mod_id, vec_of_ids) in dest_mod_to_items.iter() {
        let items: Vec<P<Item>> = vec_of_ids
            .iter()
            .map(|id| P(krate_info.item_map.get(id).unwrap().clone()))
            .collect();

        if let Some(ident) = inverse_map.get(dest_mod_id) {
            let new_item = mk().id(*dest_mod_id).mod_item(ident, mk().mod_(items));
            krate.module.items.push(new_item);
        }
    }
    krate
}

// TODO:
// There may be issues with multi-segment paths, if there is it probably best
// to use `Vec<PathSegment>` instead.
fn path_to_ident(path: &Path) -> Ident {
    Ident::from_str(&path.to_string())
}

/// Compares two `ForeignItem`'s, and assures they are the same
fn compare_foreign_items(fm_item: &ForeignItem, fm_item2: &ForeignItem) -> bool {
    fm_item.node.ast_equiv(&fm_item2.node) && fm_item.ident == fm_item2.ident
}

/// Compares an item not only using `ast_equiv`, but also in a variety of different ways
/// to handle different cases where an item may be equivalent but not caught by `ast_equiv`.
fn compare_items(new_item: &Item, module_item: &Item) -> bool {
    if new_item.node.ast_equiv(&module_item.node) && new_item.ident == module_item.ident {
        return true;
    }

    // The next two if statements are a check for constant and type alias'. This check is due to
    // the renamer on the transpiler side tacking on a number to duplicate names, and this usually
    // happens with typedefs.
    //
    // So there are times where when moving items into modules where there are two of the same
    // type, but with differing names.
    // E.g:
    // ```
    // pub type Foo: unnamed = 0;
    // pub type Foo: unnamed_0 = 0;
    // ```
    // And both unnamed and unnamed_0 are both of type `libc::uint;`, so one of these `Foo`'s must
    // be removed.
    // TODO:
    // * Assure that these two items are in fact of the same type, just to be safe.
    if let ItemKind::Ty(_, _) = new_item.node {
        if let ItemKind::Ty(_, _) = module_item.node {
            if new_item.ident == module_item.ident {
                return true;
            }
        }
    }

    if let ItemKind::Const(_, _) = new_item.node {
        if let ItemKind::Const(_, _) = module_item.node {
            if new_item.ident == module_item.ident {
                return true;
            }
        }
    }

    if let ItemKind::Use(ref new) = new_item.node {
        if let ItemKind::Use(ref mo) = module_item.node {
            let mut new_copy = new.clone();
            let mut mo_copy = mo.clone();
            new_copy.prefix.segments.retain(|segment| {
                segment.ident.name != keywords::Super.name()
                    && segment.ident.name != keywords::SelfValue.name()
            });

            mo_copy.prefix.segments.retain(|segment| {
                segment.ident.name != keywords::Super.name()
                    && segment.ident.name != keywords::SelfValue.name()
            });

            if new_copy.ast_equiv(&mo_copy) {
                return true;
            }
        }
    }
    false
}

/// Check if the `Item` has the `#[header_src = "/some/path"]` attribute
fn has_source_header(attrs: &Vec<Attribute>) -> bool {
    attr::contains_name(attrs, "header_src")
}

/// A complimentary check to `has_source_header`, that checks if the path within
/// the attribute contains `/usr/include`
// TODO: In macOS mojave the system headers aren't in `/usr/include` anymore,
// so this needs to be updated.
fn is_std(attrs: &Vec<Attribute>) -> bool {
    attrs.into_iter().any(|attr| {
        if let Some(meta) = attr.meta() {
            if let Some(value_str) = meta.value_str() {
                return value_str.as_str().contains("/usr/include");
            }
        }
        false
    })
}

impl Transform for ReorganizeDefinitions {
    fn transform(&self, krate: Crate, st: &CommandState, _cx: &driver::Ctxt) -> Crate {
        let mut krate_info = CrateInfo::new(st);

        krate_info.find_destination_modules(&krate);

        krate.visit(&mut krate_info);

        // `dest_mod_to_items`:
        // NodeId -> vec<NodeId>
        // The mapping is the destination module's `NodeId` to the items needing to be added to it.
        let dest_mod_to_items = create_dest_mod_map(&krate_info);

        // Insert a new modules into the Crate
        let krate = extend_crate(krate, &krate_info, &dest_mod_to_items);

        // Insert all the items marked as to be moved, into the proper
        // "destination module"
        let krate = krate_info.insert_items_into_dest(krate, &dest_mod_to_items);

        krate_info.item_map.clear();
        visit_nodes(&krate, |i: &Item| {
            krate_info.item_map.insert(i.id, i.clone());
        });

        let krate = deduplicate_krate(krate, &krate_info);

        krate
    }

    fn min_phase(&self) -> Phase {
        Phase::Phase3
    }
}

pub fn register_commands(reg: &mut Registry) {
    use super::mk;

    reg.register("reorganize_definitions", |_args| mk(ReorganizeDefinitions))
}
