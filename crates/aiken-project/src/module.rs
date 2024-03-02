use crate::error::Error;
use aiken_lang::{
    ast::{
        DataType, DataTypeKey, Definition, Function, FunctionAccessKey, Located, ModuleKind,
        Tracing, TypedDataType, TypedFunction, TypedModule, TypedValidator, UntypedModule,
        Validator,
    },
    gen_uplc::CodeGenerator,
    line_numbers::LineNumbers,
    parser::extra::{comments_before, Comment, ModuleExtra},
    tipo::TypeInfo,
};
use indexmap::IndexMap;
use petgraph::{algo, graph::NodeIndex, Direction, Graph};
use std::{
    collections::{HashMap, HashSet},
    ops::{Deref, DerefMut},
    path::PathBuf,
};

#[derive(Debug)]
pub struct ParsedModule {
    pub path: PathBuf,
    pub name: String,
    pub code: String,
    pub kind: ModuleKind,
    pub package: String,
    pub ast: UntypedModule,
    pub extra: ModuleExtra,
}

impl ParsedModule {
    pub fn deps_for_graph(&self) -> (String, Vec<String>) {
        let name = self.name.clone();

        let deps: Vec<_> = self
            .ast
            .dependencies()
            .into_iter()
            .map(|(dep, _span)| dep)
            .collect();

        (name, deps)
    }
}

pub struct ParsedModules(HashMap<String, ParsedModule>);

impl ParsedModules {
    pub fn sequence(&self) -> Result<Vec<String>, Error> {
        let inputs = self
            .0
            .values()
            .map(|m| m.deps_for_graph())
            .collect::<Vec<(String, Vec<String>)>>();

        let capacity = inputs.len();

        let mut graph = Graph::<(), ()>::with_capacity(capacity, capacity * 5);

        // TODO: maybe use a bimap?
        let mut indices = HashMap::with_capacity(capacity);
        let mut values = HashMap::with_capacity(capacity);

        for (value, _) in &inputs {
            let index = graph.add_node(());

            indices.insert(value.clone(), index);

            values.insert(index, value.clone());
        }

        for (value, deps) in inputs {
            if let Some(from_index) = indices.get(&value) {
                let deps = deps.into_iter().filter_map(|dep| indices.get(&dep));

                for to_index in deps {
                    graph.add_edge(*from_index, *to_index, ());
                }
            }
        }

        match algo::toposort(&graph, None) {
            Ok(sequence) => {
                let sequence = sequence
                    .iter()
                    .filter_map(|i| values.remove(i))
                    .rev()
                    .collect();

                Ok(sequence)
            }
            Err(cycle) => {
                let origin = cycle.node_id();

                let mut path = vec![];

                find_cycle(origin, origin, &graph, &mut path, &mut HashSet::new());

                let modules = path
                    .iter()
                    .filter_map(|index| values.remove(index))
                    .collect();

                Err(Error::ImportCycle { modules })
            }
        }
    }
}

impl From<HashMap<String, ParsedModule>> for ParsedModules {
    fn from(parsed_modules: HashMap<String, ParsedModule>) -> Self {
        ParsedModules(parsed_modules)
    }
}

impl From<ParsedModules> for HashMap<String, ParsedModule> {
    fn from(parsed_modules: ParsedModules) -> Self {
        parsed_modules.0
    }
}

impl Deref for ParsedModules {
    type Target = HashMap<String, ParsedModule>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ParsedModules {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

fn find_cycle(
    origin: NodeIndex,
    parent: NodeIndex,
    graph: &petgraph::Graph<(), ()>,
    path: &mut Vec<NodeIndex>,
    seen: &mut HashSet<NodeIndex>,
) -> bool {
    seen.insert(parent);

    for node in graph.neighbors_directed(parent, Direction::Outgoing) {
        if node == origin {
            path.push(node);

            return true;
        }

        if seen.contains(&node) {
            continue;
        }

        if find_cycle(origin, node, graph, path, seen) {
            path.push(node);

            return true;
        }
    }

    false
}

#[derive(Debug, Clone)]
pub struct CheckedModule {
    pub name: String,
    pub code: String,
    pub input_path: PathBuf,
    pub kind: ModuleKind,
    pub package: String,
    pub ast: TypedModule,
    pub extra: ModuleExtra,
}

impl CheckedModule {
    pub fn find_node(&self, byte_index: usize) -> Option<Located<'_>> {
        self.ast.find_node(byte_index)
    }

    pub fn attach_doc_and_module_comments(&mut self) {
        // Module Comments
        self.ast.docs = self
            .extra
            .module_comments
            .iter()
            .map(|span| {
                Comment::from((span, self.code.as_str()))
                    .content
                    .to_string()
            })
            .collect();

        // Order definitions to avoid dissociating doc comments from them
        let mut definitions: Vec<_> = self.ast.definitions.iter_mut().collect();
        definitions.sort_by(|a, b| a.location().start.cmp(&b.location().start));

        // Doc Comments
        let mut doc_comments = self.extra.doc_comments.iter().peekable();
        for def in &mut definitions {
            let docs: Vec<&str> =
                comments_before(&mut doc_comments, def.location().start, &self.code);
            if !docs.is_empty() {
                let doc = docs.join("\n");
                def.put_doc(doc);
            }

            match def {
                Definition::DataType(DataType { constructors, .. }) => {
                    for constructor in constructors {
                        let docs: Vec<&str> = comments_before(
                            &mut doc_comments,
                            constructor.location.start,
                            &self.code,
                        );
                        if !docs.is_empty() {
                            let doc = docs.join("\n");
                            constructor.put_doc(doc);
                        }

                        for argument in constructor.arguments.iter_mut() {
                            let docs: Vec<&str> = comments_before(
                                &mut doc_comments,
                                argument.location.start,
                                &self.code,
                            );
                            if !docs.is_empty() {
                                let doc = docs.join("\n");
                                argument.put_doc(doc);
                            }
                        }
                    }
                }
                Definition::Fn(Function { arguments, .. }) => {
                    for argument in arguments {
                        let docs: Vec<&str> =
                            comments_before(&mut doc_comments, argument.location.start, &self.code);

                        if !docs.is_empty() {
                            let doc = docs.join("\n");
                            argument.put_doc(doc);
                        }
                    }
                }
                Definition::Validator(Validator {
                    params,
                    fun,
                    other_fun,
                    ..
                }) => {
                    for param in params {
                        let docs: Vec<&str> =
                            comments_before(&mut doc_comments, param.location.start, &self.code);

                        if !docs.is_empty() {
                            let doc = docs.join("\n");
                            param.put_doc(doc);
                        }
                    }

                    for argument in fun.arguments.iter_mut() {
                        let docs: Vec<&str> =
                            comments_before(&mut doc_comments, argument.location.start, &self.code);

                        if !docs.is_empty() {
                            let doc = docs.join("\n");
                            argument.put_doc(doc);
                        }
                    }

                    if let Some(fun) = other_fun {
                        for argument in fun.arguments.iter_mut() {
                            let docs: Vec<&str> = comments_before(
                                &mut doc_comments,
                                argument.location.start,
                                &self.code,
                            );

                            if !docs.is_empty() {
                                let doc = docs.join("\n");
                                argument.put_doc(doc);
                            }
                        }
                    }
                }
                _ => (),
            }
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct CheckedModules(HashMap<String, CheckedModule>);

impl From<HashMap<String, CheckedModule>> for CheckedModules {
    fn from(checked_modules: HashMap<String, CheckedModule>) -> Self {
        CheckedModules(checked_modules)
    }
}

impl From<CheckedModules> for HashMap<String, CheckedModule> {
    fn from(checked_modules: CheckedModules) -> Self {
        checked_modules.0
    }
}

impl<'a> From<&'a CheckedModules> for &'a HashMap<String, CheckedModule> {
    fn from(checked_modules: &'a CheckedModules) -> Self {
        &checked_modules.0
    }
}

impl CheckedModules {
    pub fn singleton(module: CheckedModule) -> Self {
        let mut modules = Self::default();
        modules.insert(module.name.clone(), module);
        modules
    }

    pub fn validators(&self) -> impl Iterator<Item = (&CheckedModule, &TypedValidator)> {
        let mut items = vec![];

        for validator_module in self.0.values().filter(|module| module.kind.is_validator()) {
            for some_definition in validator_module.ast.definitions() {
                if let Definition::Validator(def) = some_definition {
                    items.push((validator_module, def));
                }
            }
        }

        items.sort_by(|left, right| {
            (
                left.0.package.to_string(),
                left.0.name.to_string(),
                left.1.fun.name.to_string(),
            )
                .cmp(&(
                    right.0.package.to_string(),
                    right.0.name.to_string(),
                    right.1.fun.name.to_string(),
                ))
        });

        items.into_iter()
    }

    pub fn into_validators(self) -> impl Iterator<Item = CheckedModule> {
        self.0
            .into_values()
            .filter(|module| module.kind.is_validator())
    }

    pub fn new_generator<'a>(
        &'a self,
        builtin_functions: &'a IndexMap<FunctionAccessKey, TypedFunction>,
        builtin_data_types: &'a IndexMap<DataTypeKey, TypedDataType>,
        module_types: &'a HashMap<String, TypeInfo>,
        tracing: Tracing,
    ) -> CodeGenerator<'a> {
        let mut functions = IndexMap::new();
        for (k, v) in builtin_functions {
            functions.insert(k.clone(), v);
        }

        let mut data_types = IndexMap::new();
        for (k, v) in builtin_data_types {
            data_types.insert(k.clone(), v);
        }

        let mut module_src = IndexMap::new();

        println!("Looking for modules definitions");

        for module in self.values() {
            for def in module.ast.definitions() {
                match def {
                    Definition::Fn(func) => {
                        println!("Found function: {}", func.name);
                        functions.insert(
                            FunctionAccessKey {
                                module_name: module.name.clone(),
                                function_name: func.name.clone(),
                            },
                            func,
                        );
                    }
                    Definition::DataType(dt) => {
                        data_types.insert(
                            DataTypeKey {
                                module_name: module.name.clone(),
                                defined_type: dt.name.clone(),
                            },
                            dt,
                        );
                    }

                    Definition::TypeAlias(_)
                    | Definition::ModuleConstant(_)
                    | Definition::Test(_)
                    | Definition::Validator(_)
                    | Definition::Use(_) => {}
                }
            }
            module_src.insert(
                module.name.clone(),
                (module.code.clone(), LineNumbers::new(&module.code)),
            );
        }

        let mut module_types_index = IndexMap::new();
        module_types_index.extend(module_types);

        CodeGenerator::new(
            functions,
            data_types,
            module_types_index,
            module_src,
            tracing.trace_level(true),
        )
    }
}

impl Deref for CheckedModules {
    type Target = HashMap<String, CheckedModule>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for CheckedModules {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
