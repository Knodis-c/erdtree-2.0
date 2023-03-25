use crate::render::{context::Context, disk_usage::FileSize, order::Order};
use crossbeam::channel::{self, Sender};
use error::Error;
use ignore::{WalkBuilder, WalkParallel};
use indextree::{Arena, NodeId};
use node::Node;
use std::{
    collections::{HashMap, HashSet},
    convert::TryFrom,
    fmt::{self, Display, Formatter},
    fs,
    path::PathBuf,
    thread,
};
use visitor::{BranchVisitorBuilder, TraversalState};

/// Errors related to traversal, [Tree] construction, and the like.
pub mod error;

/// Contains components of the [`Tree`] data structure that derive from [`DirEntry`].
///
/// [`Tree`]: Tree
/// [`DirEntry`]: ignore::DirEntry
pub mod node;

/// [ui::LS_COLORS] initialization and ui theme for [Tree].
pub mod ui;

/// Custom visitor that operates on each thread during filesystem traversal.
mod visitor;

/// Virtual data structure that represents local file-system hierarchy.
#[derive(Debug)]
pub struct Tree {
    inner: Arena<Node>,
    root: NodeId,
    ctx: Context,
}

pub type TreeResult<T> = Result<T, Error>;

impl Tree {
    /// Constructor for [Tree].
    pub fn new(inner: Arena<Node>, root: NodeId, ctx: Context) -> Self {
        Self { inner, root, ctx }
    }

    /// Initiates file-system traversal and [Tree construction].
    pub fn init(ctx: Context) -> TreeResult<Self> {
        let (inner, root) = Self::traverse(&ctx)?;

        Ok(Self::new(inner, root, ctx))
    }

    /// Maximum depth to display.
    fn level(&self) -> usize {
        self.ctx.level.unwrap_or(usize::MAX)
    }

    /// Grab a reference to [Context].
    fn context(&self) -> &Context {
        &self.ctx
    }

    /// Grabs a reference to `inner`.
    fn inner(&self) -> &Arena<Node> {
        &self.inner
    }

    /// Parallel traversal of the root directory and its contents. Parallel traversal relies on
    /// `WalkParallel`. Any filesystem I/O or related system calls are expected to occur during
    /// parallel traversal; post-processing post-processing of all directory entries should
    /// be completely CPU-bound.
    fn traverse(ctx: &Context) -> TreeResult<(Arena<Node>, NodeId)> {
        let (tx, rx) = channel::unbounded::<TraversalState>();

        thread::scope(|s| {
            let res = s.spawn(|| {
                let mut tree = Arena::new();
                let mut branches: HashMap<PathBuf, Vec<NodeId>> = HashMap::new();
                let mut inodes = HashSet::new();

                let mut root_id = None;

                while let Ok(TraversalState::Ongoing(node)) = rx.recv() {
                    if node.is_dir() {
                        let node_path = node.path();

                        if !branches.contains_key(node_path) {
                            branches.insert(node_path.to_owned(), vec![]);
                        }

                        if node.depth == 0 {
                            root_id = Some(tree.new_node(node));
                            continue;
                        }
                    }

                    // If a hard-link is already accounted for, skip all subsequent ones.
                    if let Some(inode) = node.inode() {
                        if inode.nlink > 1 {
                            if !inodes.insert(inode.properties()) {
                                continue;
                            }
                        }
                    }

                    let parent = node.parent_path().ok_or(Error::ExpectedParent)?.to_owned();

                    let node_id = tree.new_node(node);

                    if let None = branches
                        .get_mut(&parent)
                        .map(|mut_ref| mut_ref.push(node_id))
                    {
                        branches.insert(parent, vec![]);
                    }
                }

                let root = root_id.ok_or(Error::MissingRoot)?;

                Self::assemble_tree(&mut tree, root, &mut branches, ctx);

                if ctx.prune {
                    Self::prune_directories(root, &mut tree);
                }

                Ok::<(Arena<Node>, NodeId), Error>((tree, root))
            });

            let mut visitor_builder = BranchVisitorBuilder::new(ctx, Sender::clone(&tx));

            let walker = WalkParallel::try_from(ctx)?;

            walker.visit(&mut visitor_builder);

            tx.send(TraversalState::Done).unwrap();

            res.join().unwrap()
        })
    }

    /// Takes the results of the parallel traversal and uses it to construct the [Tree] data
    /// structure. Sorting occurs if specified.
    fn assemble_tree(
        tree: &mut Arena<Node>,
        current_node_id: NodeId,
        branches: &mut HashMap<PathBuf, Vec<NodeId>>,
        ctx: &Context,
    ) {
        let current_node = tree[current_node_id].get_mut();

        let mut children = branches.remove(current_node.path()).unwrap();

        let mut dir_size = FileSize::new(0, ctx.disk_usage, ctx.prefix, ctx.scale);

        for child_id in children.iter() {
            let index = *child_id;

            let is_dir = {
                let inner = tree[index].get();
                inner.is_dir()
            };

            if is_dir {
                Self::assemble_tree(tree, index, branches, ctx);
            }

            if let Some(file_size) = tree[index].get().file_size() {
                dir_size += file_size.bytes
            }
        }

        if dir_size.bytes > 0 {
            tree[current_node_id].get_mut().set_file_size(dir_size);
        }

        // Sort if sorting specified
        if let Some(func) = Order::from((ctx.sort(), ctx.dirs_first())).comparator() {
            children.sort_by(|id_a, id_b| {
                let node_a = tree[*id_a].get();
                let node_b = tree[*id_b].get();
                func(node_a, node_b)
            });
        }

        // Append children to current node.
        for child_id in children {
            current_node_id.append(child_id, tree);
        }
    }

    /// Function to remove empty directories.
    fn prune_directories(root_id: NodeId, tree: &mut Arena<Node>) {
        let mut to_prune = vec![];

        for node_id in root_id.descendants(tree) {
            let node = tree[node_id].get();

            if node.is_dir() {
                if node_id.children(tree).peekable().peek().is_none() {
                    to_prune.push(node_id);
                }
            }
        }

        for node_id in to_prune {
            node_id.remove_subtree(tree)
        }
    }
}

impl TryFrom<&Context> for WalkParallel {
    type Error = Error;

    fn try_from(clargs: &Context) -> Result<Self, Self::Error> {
        let root = fs::canonicalize(clargs.dir())?;

        fs::metadata(&root).map_err(|e| Error::DirNotFound(format!("{}: {e}", root.display())))?;

        Ok(WalkBuilder::new(root)
            .follow_links(clargs.follow_links)
            .git_ignore(!clargs.ignore_git_ignore)
            .hidden(!clargs.hidden)
            .threads(clargs.threads)
            .overrides(clargs.overrides()?)
            .build_parallel())
    }
}

impl Display for Tree {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let root = self.root;
        let inner = self.inner();
        let level = self.level();
        let ctx = self.context();

        let mut descendants = root.descendants(inner).skip(1).peekable();

        let root_node = inner[root].get();

        fn display_node(
            node: &Node,
            base_prefix: &str,
            ctx: &Context,
            f: &mut Formatter<'_>,
        ) -> fmt::Result {
            if ctx.size_left && !ctx.suppress_size {
                node.display_size_left(f, base_prefix, ctx)?;
            } else {
                node.display_size_right(f, base_prefix, ctx)?;
            }

            writeln!(f, "")
        }

        display_node(&root_node, "", ctx, f)?;

        let mut prefix_components = vec![""];

        while let Some(current_node_id) = descendants.next() {
            let mut current_prefix_components = prefix_components.clone();

            let current_node = inner[current_node_id].get();

            let theme = if current_node.is_symlink() {
                ui::get_link_theme()
            } else {
                ui::get_theme()
            };

            let mut siblings = current_node_id.following_siblings(inner).skip(1).peekable();

            let last_sibling = siblings.peek().is_none();

            if last_sibling {
                current_prefix_components.push(theme.get("uprt").unwrap());
            } else {
                current_prefix_components.push(theme.get("vtrt").unwrap());
            }

            let prefix = current_prefix_components.join("");

            if current_node.depth <= level {
                display_node(&current_node, &prefix, ctx, f)?;
            }

            if let Some(next_id) = descendants.peek() {
                let next_node = inner[*next_id].get();

                if next_node.depth == current_node.depth + 1 {
                    if last_sibling {
                        prefix_components.push(ui::SEP);
                    } else {
                        prefix_components.push(theme.get("vt").unwrap());
                    }
                } else if next_node.depth < current_node.depth {
                    let depth_delta = current_node.depth - next_node.depth;

                    prefix_components.truncate(prefix_components.len() - depth_delta);
                }
            }
        }

        Ok(())
    }
}
