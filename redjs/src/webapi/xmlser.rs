//! Native `XMLSerializer` (DOM Parsing and Serialization §"XMLSerializer").
//!
//! The serialization algorithm — including the namespace prefix map, generated
//! prefixes and the escaping rules — is Rust, over the [`XNode`] tree in this
//! file. The JS half only *reads* a node-shaped object into that tree, in one
//! traversal, before the algorithm runs.
//!
//! That split is the point: the traversal is the part that is temporary. This
//! runtime has no real DOM (`document.createElement` returns an inert object),
//! so today a node is whatever the caller hands over — anything exposing
//! `nodeType`, `childNodes`, `attributes`, `localName`/`prefix`/`namespaceURI`
//! and `data`. When a node tree does exist in Rust, the traversal is replaced
//! and [`serialize`] is untouched.
//!
//! ## Deliberate narrowing
//!
//! The spec's algorithm is parameterised by `require well-formed`, and the
//! well-formedness checks throw for things like an element whose `localName`
//! contains a colon. `XMLSerializer.serializeToString()` is defined as
//! "produce an XML serialization of root **given false**" (spec §2.1), so those
//! checks are out of scope here BY the specification, not by omission. The
//! `innerHTML`-setter path, which is the caller that passes true, does not
//! exist in this runtime.

use core::ffi::c_void;

use super::util::*;

pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
pub const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";
pub const HTML_NS: &str = "http://www.w3.org/1999/xhtml";

/// Elements that never get an end tag when they are in the HTML namespace.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input",
    "keygen", "link", "menuitem", "meta", "param", "source", "track", "wbr",
];

// ---------------------------------------------------------------------------
// The tree the algorithm runs on
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct XAttr {
    pub namespace: Option<String>,
    pub prefix: Option<String>,
    pub local_name: String,
    pub value: String,
}

#[derive(Clone, Debug)]
pub enum XNode {
    Element {
        namespace: Option<String>,
        prefix: Option<String>,
        local_name: String,
        attrs: Vec<XAttr>,
        children: Vec<XNode>,
    },
    Text(String),
    CData(String),
    Comment(String),
    ProcessingInstruction { target: String, data: String },
    DocumentType { name: String, public_id: String, system_id: String },
    /// Document and DocumentFragment both serialize as just their children.
    Container(Vec<XNode>),
}

// ---------------------------------------------------------------------------
// Escaping (spec §"serializing an attribute value" / text nodes)
// ---------------------------------------------------------------------------

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#xD;"),
            c => out.push(c),
        }
    }
    out
}

fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The namespace prefix map (spec §"the namespace prefix map")
// ---------------------------------------------------------------------------

/// namespace URI → the prefixes bound to it, in the order they were added.
/// A list rather than one prefix because the spec's lookup prefers a *requested*
/// prefix when it is among the candidates and falls back to the most recent.
#[derive(Clone, Default, Debug)]
struct PrefixMap {
    entries: Vec<(String, Vec<Option<String>>)>,
}

impl PrefixMap {
    fn candidates(&self, ns: &str) -> Option<&Vec<Option<String>>> {
        self.entries.iter().find(|(k, _)| k == ns).map(|(_, v)| v)
    }

    fn add(&mut self, prefix: Option<String>, ns: &str) {
        match self.entries.iter_mut().find(|(k, _)| k == ns) {
            Some((_, list)) => {
                if !list.contains(&prefix) {
                    list.push(prefix);
                }
            }
            None => self.entries.push((ns.to_string(), vec![prefix])),
        }
    }

    fn has_prefix(&self, prefix: &str) -> bool {
        self.entries.iter().any(|(_, list)| list.iter().any(|p| p.as_deref() == Some(prefix)))
    }

    /// Spec §"retrieving a preferred prefix string".
    fn preferred(&self, ns: &str, preferred: Option<&str>) -> Option<Option<String>> {
        let list = self.candidates(ns)?;
        for p in list {
            if p.as_deref() == preferred {
                return Some(p.clone());
            }
        }
        list.last().cloned()
    }
}

/// Spec §"generating a prefix": `ns1`, `ns2`, … bound to `ns`.
fn generate_prefix(map: &mut PrefixMap, ns: &str, index: &mut u32) -> String {
    let prefix = format!("ns{index}");
    *index += 1;
    map.add(Some(prefix.clone()), ns);
    prefix
}

/// Spec §"recording the namespace information". Returns the value of the
/// default-namespace declaration on this element, if any.
fn record_namespaces(
    attrs: &[XAttr],
    map: &mut PrefixMap,
    local_prefixes: &mut Vec<(String, String)>,
) -> Option<String> {
    let mut default_ns_value: Option<String> = None;
    for attr in attrs {
        if attr.namespace.as_deref() != Some(XMLNS_NS) {
            continue;
        }
        if attr.prefix.is_none() {
            // `xmlns="…"` — the default namespace declaration.
            default_ns_value = Some(attr.value.clone());
            continue;
        }
        // `xmlns:p="…"`
        let prefix_definition = attr.local_name.clone();
        let namespace_definition = attr.value.clone();
        if namespace_definition == XML_NS {
            continue;
        }
        let ns_key = namespace_definition.clone();
        let already = map
            .candidates(&ns_key)
            .map(|l| l.iter().any(|p| p.as_deref() == Some(prefix_definition.as_str())))
            .unwrap_or(false);
        if !already {
            map.add(Some(prefix_definition.clone()), &ns_key);
        }
        local_prefixes.push((prefix_definition, namespace_definition));
    }
    default_ns_value
}

// ---------------------------------------------------------------------------
// The algorithm
// ---------------------------------------------------------------------------

/// Spec §"XML serialization", with `require well-formed` false — see the module
/// docs for why that is the specified behaviour of `serializeToString`.
pub fn serialize(node: &XNode) -> String {
    let mut map = PrefixMap::default();
    map.add(Some("xml".into()), XML_NS);
    let mut index = 1u32;
    let mut out = String::new();
    serialize_node(node, None, &mut map, &mut index, &mut out);
    out
}

fn serialize_node(
    node: &XNode,
    context_ns: Option<&str>,
    map: &mut PrefixMap,
    index: &mut u32,
    out: &mut String,
) {
    match node {
        XNode::Container(children) => {
            for child in children {
                serialize_node(child, context_ns, map, index, out);
            }
        }
        XNode::Text(data) => out.push_str(&escape_text(data)),
        // CDATA is NOT escaped — that is the whole point of the section — but a
        // `]]>` inside it would terminate it early, so it is split, which is
        // what every serializer does and what keeps the output parseable.
        XNode::CData(data) => {
            out.push_str("<![CDATA[");
            out.push_str(&data.replace("]]>", "]]]]><![CDATA[>"));
            out.push_str("]]>");
        }
        XNode::Comment(data) => {
            out.push_str("<!--");
            out.push_str(data);
            out.push_str("-->");
        }
        XNode::ProcessingInstruction { target, data } => {
            out.push_str("<?");
            out.push_str(target);
            out.push(' ');
            out.push_str(data);
            out.push_str("?>");
        }
        XNode::DocumentType { name, public_id, system_id } => {
            out.push_str("<!DOCTYPE ");
            out.push_str(name);
            if !public_id.is_empty() {
                out.push_str(" PUBLIC \"");
                out.push_str(public_id);
                out.push('"');
            } else if !system_id.is_empty() {
                out.push_str(" SYSTEM");
            }
            if !system_id.is_empty() {
                out.push_str(" \"");
                out.push_str(system_id);
                out.push('"');
            }
            out.push('>');
        }
        XNode::Element { namespace, prefix, local_name, attrs, children } => {
            serialize_element(namespace, prefix, local_name, attrs, children, context_ns, map, index, out);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn serialize_element(
    namespace: &Option<String>,
    prefix: &Option<String>,
    local_name: &str,
    attrs: &[XAttr],
    children: &[XNode],
    context_ns: Option<&str>,
    map: &mut PrefixMap,
    index: &mut u32,
    out: &mut String,
) {
    out.push('<');
    let qualified_name: String;
    let mut skip_end_tag = false;
    let mut ignore_ns_def_attr = false;
    let mut local_map = map.clone();
    let mut local_prefixes: Vec<(String, String)> = Vec::new();
    let local_default_ns = record_namespaces(attrs, &mut local_map, &mut local_prefixes);
    let mut inherited_ns: Option<String> = context_ns.map(|s| s.to_string());
    let ns = namespace.clone();

    if inherited_ns.as_deref() == ns.as_deref() {
        if local_default_ns.is_some() {
            ignore_ns_def_attr = true;
        }
        qualified_name = if prefix.as_deref() == Some("xml") {
            format!("xml:{local_name}")
        } else {
            local_name.to_string()
        };
        out.push_str(&qualified_name);
    } else {
        let ns_str = ns.clone().unwrap_or_default();
        let candidate = local_map.preferred(&ns_str, prefix.as_deref()).flatten();
        if let Some(cand) = candidate {
            qualified_name = format!("{cand}:{local_name}");
            if let Some(d) = &local_default_ns {
                if d != XML_NS {
                    inherited_ns = if d.is_empty() { None } else { Some(d.clone()) };
                }
            }
            out.push_str(&qualified_name);
        } else if let Some(pfx) = prefix.clone() {
            // The element asked for a prefix. Reuse it unless this element has
            // just rebound it to something else, in which case invent one —
            // reusing it there would silently retag the element.
            let pfx = if local_prefixes.iter().any(|(p, _)| *p == pfx) {
                generate_prefix(&mut local_map, &ns_str, index)
            } else {
                local_map.add(Some(pfx.clone()), &ns_str);
                pfx
            };
            qualified_name = format!("{pfx}:{local_name}");
            out.push_str(&qualified_name);
            out.push_str(" xmlns:");
            out.push_str(&pfx);
            out.push_str("=\"");
            out.push_str(&escape_attr(&ns_str));
            out.push('"');
            if let Some(d) = &local_default_ns {
                inherited_ns = if d.is_empty() { None } else { Some(d.clone()) };
            }
        } else if local_default_ns.is_none() || local_default_ns.as_deref() != ns.as_deref() {
            ignore_ns_def_attr = true;
            qualified_name = local_name.to_string();
            inherited_ns = ns.clone();
            out.push_str(&qualified_name);
            out.push_str(" xmlns=\"");
            out.push_str(&escape_attr(&ns_str));
            out.push('"');
        } else {
            qualified_name = local_name.to_string();
            inherited_ns = ns.clone();
            out.push_str(&qualified_name);
        }
    }

    out.push_str(&serialize_attributes(attrs, &mut local_map, index, &local_prefixes, ignore_ns_def_attr));

    let html = ns.as_deref() == Some(HTML_NS);
    if children.is_empty() {
        if html && VOID_ELEMENTS.contains(&local_name) {
            out.push_str(" /");
            skip_end_tag = true;
        } else if !html {
            out.push('/');
            skip_end_tag = true;
        }
    }
    out.push('>');
    if skip_end_tag {
        return;
    }

    for child in children {
        serialize_node(child, inherited_ns.as_deref(), &mut local_map, index, out);
    }
    out.push_str("</");
    out.push_str(&qualified_name);
    out.push('>');
}

/// Spec §"XML serialization of the attributes of an element node".
fn serialize_attributes(
    attrs: &[XAttr],
    map: &mut PrefixMap,
    index: &mut u32,
    local_prefixes: &[(String, String)],
    ignore_ns_def_attr: bool,
) -> String {
    let mut result = String::new();
    for attr in attrs {
        let mut candidate_prefix: Option<String> = None;
        if let Some(attr_ns) = &attr.namespace {
            candidate_prefix = map.preferred(attr_ns, attr.prefix.as_deref()).flatten();
            if attr_ns == XMLNS_NS {
                if attr.value == XML_NS {
                    continue;
                }
                if attr.prefix.is_none() && ignore_ns_def_attr {
                    continue;
                }
                candidate_prefix = Some("xmlns".into());
            } else if candidate_prefix.is_none() {
                let pfx = match &attr.prefix {
                    Some(p) if !map.has_prefix(p) => {
                        map.add(Some(p.clone()), attr_ns);
                        p.clone()
                    }
                    _ => generate_prefix(map, attr_ns, index),
                };
                candidate_prefix = Some(pfx.clone());
                result.push_str(" xmlns:");
                result.push_str(&pfx);
                result.push_str("=\"");
                result.push_str(&escape_attr(attr_ns));
                result.push('"');
            }
        }
        let _ = local_prefixes;
        result.push(' ');
        if let Some(p) = &candidate_prefix {
            // A default `xmlns="…"` declaration has no prefix on the wire.
            if !(p == "xmlns" && attr.prefix.is_none()) {
                result.push_str(p);
                result.push(':');
            }
        }
        result.push_str(if attr.namespace.as_deref() == Some(XMLNS_NS) && attr.prefix.is_none() {
            "xmlns"
        } else {
            &attr.local_name
        });
        result.push_str("=\"");
        result.push_str(&escape_attr(&attr.value));
        result.push('"');
    }
    result
}

// ---------------------------------------------------------------------------
// Reading a JS node into the tree
// ---------------------------------------------------------------------------

unsafe fn opt_string(g: *mut c_void, v: u64) -> Option<String> {
    if v == UNDEF || v == NULL {
        return None;
    }
    read_string(g, v)
}

unsafe fn string_or_empty(g: *mut c_void, v: u64) -> String {
    opt_string(g, v).unwrap_or_default()
}

/// Prefix and local name, falling back to splitting `nodeName` when the node
/// does not expose them separately (the common case for hand-built trees).
unsafe fn names(g: *mut c_void, node: u64) -> (Option<String>, String) {
    let prefix = opt_string(g, get(g, node, "prefix"));
    if let Some(local) = opt_string(g, get(g, node, "localName")) {
        return (prefix, local);
    }
    let node_name = string_or_empty(g, get(g, node, "nodeName"));
    match node_name.split_once(':') {
        Some((p, l)) => (prefix.or_else(|| Some(p.to_string())), l.to_string()),
        None => (prefix, node_name),
    }
}

unsafe fn read_attrs(g: *mut c_void, node: u64) -> Vec<XAttr> {
    let list = get(g, node, "attributes");
    if !is_cell(list) {
        return Vec::new();
    }
    let n = match dec_f64(get(g, list, "length")) {
        Some(v) => v as u32,
        None => return Vec::new(),
    };
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let a = get_i(g, list, i);
        if !is_cell(a) {
            continue;
        }
        let (prefix, local_name) = names(g, a);
        // `nodeName`-derived local names cover attributes built by hand, where
        // only `name` is set.
        let local_name = if local_name.is_empty() {
            let name = string_or_empty(g, get(g, a, "name"));
            match name.split_once(':') {
                Some((_, l)) => l.to_string(),
                None => name,
            }
        } else {
            local_name
        };
        let prefix = prefix.or_else(|| {
            let name = string_or_empty(g, get(g, a, "name"));
            name.split_once(':').map(|(p, _)| p.to_string())
        });
        out.push(XAttr {
            namespace: opt_string(g, get(g, a, "namespaceURI")),
            prefix,
            local_name,
            value: string_or_empty(g, get(g, a, "value")),
        });
    }
    out
}

unsafe fn read_children(g: *mut c_void, node: u64, depth: u32) -> Vec<XNode> {
    let list = get(g, node, "childNodes");
    if !is_cell(list) {
        return Vec::new();
    }
    let n = match dec_f64(get(g, list, "length")) {
        Some(v) => v as u32,
        None => return Vec::new(),
    };
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        if let Some(child) = read_node(g, get_i(g, list, i), depth + 1) {
            out.push(child);
        }
    }
    out
}

/// A cycle in a hand-built "tree" would otherwise recurse until the stack dies,
/// and a stack overflow in a native host function takes the process with it.
const MAX_DEPTH: u32 = 512;

pub unsafe fn read_node(g: *mut c_void, node: u64, depth: u32) -> Option<XNode> {
    if !is_cell(node) || depth > MAX_DEPTH {
        return None;
    }
    let node_type = dec_f64(get(g, node, "nodeType")).unwrap_or(0.0) as i32;
    Some(match node_type {
        1 => {
            let (prefix, local_name) = names(g, node);
            XNode::Element {
                namespace: opt_string(g, get(g, node, "namespaceURI")),
                prefix,
                local_name,
                attrs: read_attrs(g, node),
                children: read_children(g, node, depth),
            }
        }
        3 => XNode::Text(string_or_empty(g, get(g, node, "data"))),
        4 => XNode::CData(string_or_empty(g, get(g, node, "data"))),
        7 => XNode::ProcessingInstruction {
            target: string_or_empty(g, get(g, node, "target")),
            data: string_or_empty(g, get(g, node, "data")),
        },
        8 => XNode::Comment(string_or_empty(g, get(g, node, "data"))),
        9 | 11 => XNode::Container(read_children(g, node, depth)),
        10 => XNode::DocumentType {
            name: string_or_empty(g, get(g, node, "name")),
            public_id: string_or_empty(g, get(g, node, "publicId")),
            system_id: string_or_empty(g, get(g, node, "systemId")),
        },
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// The JS class
// ---------------------------------------------------------------------------

#[crate::host_fn]
unsafe fn construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let proto = {
        let nt = cf_new_target(cf);
        let p = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
        if is_cell(p) {
            p
        } else {
            get(g, get(g, crate::glue::rjs_global(g), "XMLSerializer"), "prototype")
        }
    };
    new_object(g, proto)
}

#[crate::host_fn]
unsafe fn serialize_to_string(g: *mut c_void, cf: *mut c_void) -> u64 {
    let root = cf_arg(cf, 0);
    match read_node(g, root, 0) {
        Some(tree) => js_string(g, &serialize(&tree)),
        None => throw_type_error(
            g,
            "XMLSerializer.serializeToString: the argument is not a node — it must expose a \
             numeric `nodeType` (1 element, 3 text, 4 CDATA, 7 PI, 8 comment, 9 document, \
             10 doctype, 11 fragment).",
        ),
    }
}

pub unsafe fn install(g: *mut c_void) {
    define_class(g, &ClassSpec {
        name: "XMLSerializer",
        arity: 0,
        construct: Some(construct),
        parent_proto: 0,
        methods: &[("serializeToString", 1, serialize_to_string)],
        getters: &[],
        accessors: &[],
    });
}

// ---------------------------------------------------------------------------
// Tests — the algorithm, no JS context needed
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn el(local: &str, children: Vec<XNode>) -> XNode {
        XNode::Element {
            namespace: None,
            prefix: None,
            local_name: local.into(),
            attrs: vec![],
            children,
        }
    }

    fn el_ns(ns: &str, prefix: Option<&str>, local: &str, attrs: Vec<XAttr>, children: Vec<XNode>) -> XNode {
        XNode::Element {
            namespace: Some(ns.into()),
            prefix: prefix.map(|s| s.into()),
            local_name: local.into(),
            attrs,
            children,
        }
    }

    fn attr(ns: Option<&str>, prefix: Option<&str>, local: &str, value: &str) -> XAttr {
        XAttr {
            namespace: ns.map(|s| s.into()),
            prefix: prefix.map(|s| s.into()),
            local_name: local.into(),
            value: value.into(),
        }
    }

    #[test]
    fn an_empty_element_self_closes_outside_html() {
        assert_eq!(serialize(&el("a", vec![])), "<a/>");
    }

    #[test]
    fn a_void_html_element_gets_the_space_slash_form() {
        assert_eq!(serialize(&el_ns(HTML_NS, None, "br", vec![], vec![])), "<br xmlns=\"http://www.w3.org/1999/xhtml\" />");
    }

    #[test]
    fn a_non_void_html_element_keeps_its_end_tag() {
        // `<div/>` is not valid HTML and browsers parse it as an OPEN div, so
        // getting this wrong swallows the rest of the document.
        let s = serialize(&el_ns(HTML_NS, None, "div", vec![], vec![]));
        assert!(s.ends_with("></div>"), "{s}");
    }

    #[test]
    fn text_escapes_the_three_markup_characters() {
        assert_eq!(serialize(&el("a", vec![XNode::Text("a<b>&c".into())])), "<a>a&lt;b&gt;&amp;c</a>");
    }

    #[test]
    fn attribute_values_escape_quotes_and_newlines() {
        // An unescaped quote closes the attribute and turns the rest of the
        // value into markup — the injection this escaping exists to stop.
        let e = el_ns("u:n", None, "a", vec![attr(None, None, "t", "x\"y\nz")], vec![]);
        let s = serialize(&e);
        assert!(s.contains("t=\"x&quot;y&#xA;z\""), "{s}");
    }

    #[test]
    fn a_namespaced_element_declares_its_namespace_once() {
        let inner = el_ns("urn:x", None, "child", vec![], vec![]);
        let outer = el_ns("urn:x", None, "root", vec![], vec![inner]);
        // The child inherits the default namespace and must NOT redeclare it.
        assert_eq!(serialize(&outer), "<root xmlns=\"urn:x\"><child/></root>");
    }

    #[test]
    fn a_child_in_a_different_namespace_declares_its_own() {
        let inner = el_ns("urn:y", None, "child", vec![], vec![]);
        let outer = el_ns("urn:x", None, "root", vec![], vec![inner]);
        assert_eq!(serialize(&outer), "<root xmlns=\"urn:x\"><child xmlns=\"urn:y\"/></root>");
    }

    #[test]
    fn a_prefixed_element_emits_its_prefix_declaration() {
        let e = el_ns("urn:x", Some("p"), "root", vec![], vec![]);
        assert_eq!(serialize(&e), "<p:root xmlns:p=\"urn:x\"/>");
    }

    #[test]
    fn an_existing_prefix_declaration_is_reused_by_descendants() {
        let inner = el_ns("urn:x", Some("p"), "child", vec![], vec![]);
        let outer = el_ns("urn:x", Some("p"), "root", vec![], vec![inner]);
        let s = serialize(&outer);
        assert_eq!(s.matches("xmlns:p=").count(), 1, "the prefix was redeclared: {s}");
        assert_eq!(s, "<p:root xmlns:p=\"urn:x\"><p:child/></p:root>");
    }

    #[test]
    fn a_namespaced_attribute_gets_a_generated_prefix_when_it_has_none() {
        let e = el("a", vec![]);
        let XNode::Element { .. } = e else { unreachable!() };
        let e = el_ns("urn:x", None, "a", vec![attr(Some("urn:z"), None, "k", "v")], vec![]);
        let s = serialize(&e);
        assert!(s.contains("xmlns:ns1=\"urn:z\""), "{s}");
        assert!(s.contains("ns1:k=\"v\""), "{s}");
    }

    #[test]
    fn the_xml_prefix_is_predeclared_and_never_emitted() {
        // `xml:` is bound by definition (XML Namespaces §3) and declaring it
        // is an error, so a serializer that emits xmlns:xml produces invalid XML.
        let e = el_ns("urn:x", None, "a", vec![attr(Some(XML_NS), Some("xml"), "lang", "en")], vec![]);
        let s = serialize(&e);
        assert!(s.contains("xml:lang=\"en\""), "{s}");
        assert!(!s.contains("xmlns:xml"), "{s}");
    }

    #[test]
    fn an_explicit_xmlns_attribute_is_not_duplicated() {
        let e = el_ns(
            "urn:x",
            None,
            "root",
            vec![attr(Some(XMLNS_NS), None, "xmlns", "urn:x")],
            vec![],
        );
        let s = serialize(&e);
        assert_eq!(s.matches("xmlns=").count(), 1, "declared twice: {s}");
        assert_eq!(s, "<root xmlns=\"urn:x\"/>");
    }

    #[test]
    fn comments_pis_and_cdata_round_trip() {
        let tree = XNode::Container(vec![
            XNode::Comment(" hi ".into()),
            XNode::ProcessingInstruction { target: "xml-stylesheet".into(), data: "href=\"a\"".into() },
            el("a", vec![XNode::CData("raw <not> escaped".into())]),
        ]);
        assert_eq!(
            serialize(&tree),
            "<!-- hi --><?xml-stylesheet href=\"a\"?><a><![CDATA[raw <not> escaped]]></a>"
        );
    }

    #[test]
    fn a_cdata_terminator_inside_cdata_is_split_not_dropped() {
        // Emitting `]]>` verbatim ends the section early and silently turns the
        // remainder into markup.
        let s = serialize(&el("a", vec![XNode::CData("x]]>y".into())]));
        assert_eq!(s, "<a><![CDATA[x]]]]><![CDATA[>y]]></a>");
    }

    #[test]
    fn a_doctype_serializes_with_its_identifiers() {
        let d = XNode::DocumentType {
            name: "html".into(),
            public_id: "-//W3C//DTD XHTML 1.0//EN".into(),
            system_id: "http://www.w3.org/TR/xhtml1.dtd".into(),
        };
        assert_eq!(
            serialize(&d),
            "<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0//EN\" \"http://www.w3.org/TR/xhtml1.dtd\">"
        );
    }

    #[test]
    fn nesting_order_and_mixed_content_are_preserved() {
        let tree = el(
            "p",
            vec![XNode::Text("a".into()), el("b", vec![XNode::Text("c".into())]), XNode::Text("d".into())],
        );
        assert_eq!(serialize(&tree), "<p>a<b>c</b>d</p>");
    }
}

#[cfg(test)]
mod jsc_tests {
    use crate::webapi::testing::*;

    /// A tiny node factory in JS — this runtime has no DOM, so the serializer's
    /// input is whatever the caller builds. Exactly the duck-typing contract.
    const NODES: &str = "
      globalThis.txt = d => ({ nodeType: 3, data: d });
      globalThis.el = (name, attrs, kids, ns) => ({
        nodeType: 1, nodeName: name, localName: name.split(':').pop(),
        prefix: name.includes(':') ? name.split(':')[0] : null,
        namespaceURI: ns ?? null,
        attributes: (attrs || []).map(([n, v, ans]) => ({ name: n, localName: n.split(':').pop(),
          prefix: n.includes(':') ? n.split(':')[0] : null, namespaceURI: ans ?? null, value: v })),
        childNodes: kids || [],
      });
    ";

    #[test]
    fn it_serializes_a_duck_typed_tree() {
        let c = ctx();
        s(&c, NODES);
        let out = s(
            &c,
            "new XMLSerializer().serializeToString(el('root', [['id','1']], [el('kid', [], [txt('hi & bye')])]))",
        );
        assert_eq!(out, "<root id=\"1\"><kid>hi &amp; bye</kid></root>");
    }

    #[test]
    fn a_non_node_argument_says_what_it_needed() {
        let c = ctx();
        let msg = throws(&c, "new XMLSerializer().serializeToString({})");
        assert!(msg.contains("nodeType"), "{msg}");
    }

    #[test]
    fn a_cyclic_tree_is_bounded_rather_than_crashing_the_process() {
        // A stack overflow inside a native host function takes the whole
        // process down, so the depth cap is load-bearing, not tidiness.
        let c = ctx();
        s(&c, NODES);
        s(
            &c,
            "globalThis.loop = el('a', [], []); globalThis.loop.childNodes = [globalThis.loop]; 'ok'",
        );
        let out = s(&c, "new XMLSerializer().serializeToString(globalThis.loop).length > 0");
        assert_eq!(out, "true");
    }
}
