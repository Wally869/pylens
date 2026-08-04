//! Static import-cataloguing tests. Pure parsing — no jail, always run.

use pylens::imports_of;
use pylens::model::ImportScope;

#[test]
fn parses_all_import_styles() {
    let src = "\
import os\n\
import numpy as np\n\
import xml.etree.ElementTree as ET\n\
from collections import OrderedDict, defaultdict\n\
from json import dumps\n\
from os.path import *\n\
from . import sibling\n\
from .pkg import thing\n";
    let imps = imports_of(src).unwrap();

    // plain: package only, no path, no alias
    let os = imps.iter().find(|i| !i.from && i.module.package == "os").unwrap();
    assert!(os.module.path.is_empty() && os.alias.is_none());
    assert_eq!(os.bindings(), vec!["os".to_string()]);

    // alias: binds the alias, not the package
    let np = imps
        .iter()
        .find(|i| !i.from && i.module.package == "numpy")
        .unwrap();
    assert_eq!(np.alias.as_deref(), Some("np"));
    assert_eq!(np.bindings(), vec!["np".to_string()]);

    // dotted path split into package + path; alias binds ET
    let et = imps
        .iter()
        .find(|i| !i.from && i.module.package == "xml")
        .unwrap();
    assert_eq!(et.module.path, "etree.ElementTree");
    assert_eq!(et.module.dotted(), "xml.etree.ElementTree");
    assert_eq!(et.bindings(), vec!["ET".to_string()]);

    // several names from one lib → one binding each
    let coll = imps
        .iter()
        .find(|i| i.from && i.module.package == "collections")
        .unwrap();
    assert_eq!(coll.names.len(), 2);
    assert_eq!(
        coll.bindings(),
        vec!["OrderedDict".to_string(), "defaultdict".to_string()]
    );

    // star: package "os", path "path", wildcard binding (empty)
    let star = imps
        .iter()
        .find(|i| i.from && i.module.package == "os" && i.star)
        .unwrap();
    assert_eq!(star.module.path, "path");
    assert!(star.bindings().is_empty());

    // bare relative `from . import sibling`: level 1, empty module
    assert!(imps.iter().any(|i| i.from && i.level == 1 && i.module.is_empty()));
    // relative with module `from .pkg import thing`
    assert!(imps.iter().any(|i| i.from && i.level == 1 && i.module.package == "pkg"));

    // everything here is at module scope
    assert!(imps.iter().all(|i| matches!(i.scope, ImportScope::Module)));
}

#[test]
fn collects_imports_nested_in_functions() {
    let src = "def f():\n    import matplotlib\n    return 1\n";
    let imps = imports_of(src).unwrap();
    let m = imps.iter().find(|i| i.module.package == "matplotlib").unwrap();
    // an import inside a function body runs only when the function is called
    assert!(matches!(m.scope, ImportScope::Function));
}
