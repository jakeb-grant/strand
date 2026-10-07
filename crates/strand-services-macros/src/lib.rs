//! Derives and the `#[service]` attribute of `strand-services` (design.md,
//! "System services": a service is `#[service(name = "battery")]
//! #[derive(Store)]` with an async `run(cx)`).
//!
//! - `#[derive(Store)]` on a service's state struct: a `Send` patch enum
//!   (`<Name>Patch`: one variant per field, a keyed list's `VecDiff`s, one
//!   variant per event), the logic-side cells (`<Name>Cells`: a core
//!   `Signal` per field, a `KeyedSignal` per `#[store(keyed)]` list, an
//!   `EventQueue` per `Event<T>` field) and `strand_services::Store` /
//!   `Cells` impls tying them together.
//! - `#[derive(Data)]` on a record struct or a unit enum: conversion to
//!   and from `strand_services::Data` and the schema type name;
//!   `#[data(key = id)]` also implements `Keyed`.
//! - `#[derive(Call)]` on an enum of actions or async methods: typed
//!   from a call's name, item and arguments.
//! - `#[service(name = "…", schema = …)]` implements `Service`.
//!
//! The generated code names `::strand_services`, so the crate using them
//! depends on `strand-services` (which re-exports these macros).

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as Tokens};
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{
    Attribute, Data, DeriveInput, Expr, Fields, GenericArgument, Ident, LitStr, Path,
    PathArguments, Type, parse_macro_input,
};

fn err(span: Span, msg: &str) -> TokenStream {
    syn::Error::new(span, msg).to_compile_error().into()
}

/// The `///` docs of an item, one space of indent removed per line.
fn docs(attrs: &[Attribute]) -> String {
    let mut lines = Vec::new();
    for a in attrs {
        if !a.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &a.meta
            && let Expr::Lit(lit) = &nv.value
            && let syn::Lit::Str(s) = &lit.lit
        {
            let v = s.value();
            lines.push(v.strip_prefix(' ').unwrap_or(&v).to_string());
        }
    }
    lines.join("\n")
}

/// `received_at` → `ReceivedAt`.
fn camel(name: &str) -> String {
    let mut out = String::new();
    let mut up = true;
    for c in name.trim_start_matches("r#").chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// `PlayPause` → `play_pause`.
fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The single generic argument of `Outer<T>` when the type's last path
/// segment is `outer`.
fn inner_of<'a>(ty: &'a Type, outer: &str) -> Option<&'a Type> {
    let Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    if seg.ident != outer {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    match args.args.first()? {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    }
}

/// A field's name as the schema spells it (a raw identifier without its
/// `r#`).
fn field_name(id: &Ident) -> String {
    id.to_string().trim_start_matches("r#").to_string()
}

#[derive(Default)]
struct StoreFieldAttr {
    rw: bool,
    keyed: bool,
    stream: bool,
}

fn store_attr(attrs: &[Attribute]) -> syn::Result<StoreFieldAttr> {
    let mut out = StoreFieldAttr::default();
    for a in attrs {
        if !a.path().is_ident("store") {
            continue;
        }
        a.parse_nested_meta(|m| {
            if m.path.is_ident("rw") {
                out.rw = true;
                Ok(())
            } else if m.path.is_ident("keyed") {
                out.keyed = true;
                Ok(())
            } else if m.path.is_ident("stream") {
                out.stream = true;
                Ok(())
            } else {
                Err(m.error("expected `rw`, `keyed` or `stream`"))
            }
        })?;
    }
    Ok(out)
}

enum Kind<'a> {
    Plain,
    Keyed(&'a Type),
    Event(&'a Type),
}

/// The arguments an event payload type carries: `()` none, a tuple its
/// elements, anything else itself.
fn event_args(payload: &Type, value: &Tokens) -> (usize, Tokens) {
    match payload {
        Type::Tuple(t) => {
            let n = t.elems.len();
            let parts = (0..n).map(|i| {
                let idx = syn::Index::from(i);
                quote!(::strand_services::ToData::to_data(&#value.#idx))
            });
            (n, quote!(vec![#(#parts),*]))
        }
        _ => (1, quote!(vec![::strand_services::ToData::to_data(#value)])),
    }
}

/// `#[derive(Store)]`: see the crate docs. Field attributes:
/// `#[store(rw)]` marks a writable field (`<->`, assignment),
/// `#[store(keyed)]` a `Vec<T>` of `Keyed` records published as a keyed
/// collection, `#[store(stream)]` a field the service produces only while
/// a visible reader reads that field (a Wi-Fi scan, audio levels:
/// `Cx::watched`); a field of type `Event<T>` is an event (`T` its
/// payload: `()` for none, a tuple for several arguments). The derive
/// also generates `<Name>Event`, one variant per event, which
/// `Cx::emit` takes.
#[proc_macro_derive(Store, attributes(store))]
pub fn derive_store(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match store(&input) {
        Ok(t) => t.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn store(input: &DeriveInput) -> syn::Result<Tokens> {
    let name = &input.ident;
    let vis = &input.vis;
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(
            input.span(),
            "Store derives only on a struct with named fields",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new(
            input.span(),
            "Store derives only on a struct with named fields",
        ));
    };
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new(
            input.generics.span(),
            "a Store has no generic parameters",
        ));
    }
    let patch = format_ident!("{}Patch", name);
    let cells = format_ident!("{}Cells", name);
    let event = format_ident!("{}Event", name);
    let sv = quote!(::strand_services);

    let mut variants = Vec::new();
    let mut cell_fields = Vec::new();
    let mut cell_new = Vec::new();
    let mut infos = Vec::new();
    let mut event_infos = Vec::new();
    let mut diffs = Vec::new();
    let mut applies_plain = Vec::new();
    let mut targets = Vec::new();
    let mut cell_apply = Vec::new();
    let mut snapshot = Vec::new();
    let mut reads = Vec::new();
    let mut ids = Vec::new();
    let mut writes = Vec::new();
    let mut keyed_items = Vec::new();
    let mut item_writes = Vec::new();
    let mut item_patches = Vec::new();
    let mut forgets = Vec::new();
    let mut disposes = Vec::new();
    let mut debug = Vec::new();
    let mut field_patches = Vec::new();
    let mut event_variants = Vec::new();
    let mut event_into = Vec::new();

    let (mut fi, mut ei) = (0usize, 0usize);
    for f in &fields.named {
        let Some(id) = &f.ident else { continue };
        let fname = field_name(id);
        let ty = &f.ty;
        let attr = store_attr(&f.attrs)?;
        let doc = docs(&f.attrs);
        let var = format_ident!("{}", camel(&fname));
        let kind = if let Some(p) = inner_of(ty, "Event") {
            Kind::Event(p)
        } else if attr.keyed {
            match inner_of(ty, "Vec") {
                Some(t) => Kind::Keyed(t),
                None => {
                    return Err(syn::Error::new(
                        ty.span(),
                        "#[store(keyed)] needs a `Vec<T>` of `Keyed` records",
                    ));
                }
            }
        } else {
            Kind::Plain
        };
        debug.push(quote!(.field(#fname, &self.#id.id())));
        disposes.push(quote!(self.#id.dispose(rt);));
        match kind {
            Kind::Plain => {
                let i = fi;
                fi += 1;
                let (rw, stream) = (attr.rw, attr.stream);
                variants.push(quote!(#[doc = #doc] #var(#ty)));
                cell_fields.push(quote!(pub #id: #sv::core::Signal<#ty>));
                cell_new.push(quote!(#id: {
                    let s = rt.signal(::core::clone::Clone::clone(&initial.#id));
                    rt.set_name(s.id(), ::std::format!("{}.{}", service, #fname));
                    s
                }));
                infos.push(quote!(#sv::FieldInfo {
                    name: #fname,
                    ty: <#ty as #sv::SchemaType>::schema_type,
                    rw: #rw,
                    keyed: false,
                    key: ::core::option::Option::None,
                    stream: #stream,
                    doc: #doc,
                }));
                diffs.push(quote!(if old.#id != new.#id {
                    out.push(#patch::#var(::core::clone::Clone::clone(&new.#id)));
                }));
                applies_plain
                    .push(quote!(#patch::#var(v) => self.#id = ::core::clone::Clone::clone(v),));
                field_patches.push(quote!(#i => ::core::option::Option::Some(
                    #patch::#var(::core::clone::Clone::clone(&self.#id))
                ),));
                targets.push(quote!(#patch::#var(_) => #sv::Target::Field(#i),));
                forgets.push(quote!(self.#id.forget_echoes(rt);));
                cell_apply.push(quote!(#patch::#var(v) => {
                    match how {
                        // A boot read from before a local write still in
                        // flight: the write's answer comes next, and the
                        // local value stays until then (no snap-back).
                        #sv::How::Initial if self.#id.pending_writes(rt) > 0 => {}
                        #sv::How::Initial => {
                            self.#id.set_reloaded(rt, ::core::clone::Clone::clone(v))?;
                        }
                        #sv::How::Report(echo) => {
                            self.#id.receive(rt, ::core::clone::Clone::clone(v), echo)?;
                        }
                    }
                    ::core::result::Result::Ok(::core::option::Option::None)
                }));
                snapshot.push(quote!(#id: self.#id.get_untracked(rt)?));
                reads.push(quote!(#i => self.#id.with(rt, #sv::ToData::to_data),));
                ids.push(quote!(#i => ::std::vec![self.#id.id()],));
                writes.push(quote!(#i => {
                    let v = <#ty as #sv::FromData>::from_data(value)
                        .map_err(|e| #sv::core::Error::failed(::std::format!("{}: {e}", #fname)))?;
                    self.#id.write_tagged(rt, v, move |rt, _, g| send(rt, g))?;
                    ::core::result::Result::Ok(())
                }));
            }
            Kind::Keyed(item) => {
                let i = fi;
                fi += 1;
                let key = quote!(<#item as #sv::Keyed>::Key);
                let stream = attr.stream;
                variants.push(
                    quote!(#[doc = #doc] #var(::std::vec::Vec<#sv::core::VecDiff<#key, #item>>)),
                );
                cell_fields.push(quote!(pub #id: #sv::core::KeyedSignal<#key, #item>));
                cell_new.push(quote!(#id: {
                    let s = rt.keyed(#sv::keyed_vec_of(::core::clone::Clone::clone(&initial.#id)));
                    rt.set_name(s.id(), ::std::format!("{}.{}", service, #fname));
                    s
                }));
                infos.push(quote!(#sv::FieldInfo {
                    name: #fname,
                    ty: <::std::vec::Vec<#item> as #sv::SchemaType>::schema_type,
                    rw: false,
                    keyed: true,
                    key: ::core::option::Option::Some(<#item as #sv::Keyed>::KEY_FIELD),
                    stream: #stream,
                    doc: #doc,
                }));
                diffs.push(quote!({
                    let d = #sv::keyed_changes(&old.#id, &new.#id);
                    if !d.is_empty() {
                        out.push(#patch::#var(d));
                    }
                }));
                applies_plain.push(quote!(#patch::#var(d) => #sv::apply_keyed(&mut self.#id, d),));
                field_patches.push(quote!(#i => ::core::option::Option::Some(#patch::#var(::std::vec![
                    #sv::core::VecDiff::Reset {
                        items: self.#id.iter().map(|t| (#sv::Keyed::key(t), ::core::clone::Clone::clone(t))).collect(),
                    }
                ])),));
                targets.push(quote!(#patch::#var(_) => #sv::Target::Field(#i),));
                forgets.push(quote!(self.#id.forget_echoes(rt);));
                cell_apply.push(quote!(#patch::#var(d) => {
                    let applied = match how {
                        #sv::How::Initial => {
                            let mut items = self.#id.with_untracked(rt, |v| {
                                v.items().iter().map(|(_, t)| ::core::clone::Clone::clone(t)).collect::<::std::vec::Vec<_>>()
                            })?;
                            #sv::apply_keyed(&mut items, d);
                            // Items written locally since the boot read
                            // stay as written.
                            self.#id.keep_pending_items(rt, &mut items)?;
                            self.#id.replace_all_reloaded(rt, items)?;
                            d.iter().map(#sv::diff_data).collect()
                        }
                        #sv::How::Report(echo) => self
                            .#id
                            .receive_items(rt, d, echo)?
                            .iter()
                            .map(#sv::diff_data)
                            .collect(),
                    };
                    ::core::result::Result::Ok(::core::option::Option::Some(#sv::Applied::Keyed {
                        field: #i,
                        diffs: applied,
                        initial: how == #sv::How::Initial,
                    }))
                }));
                item_writes.push(quote!(#i => {
                    let found = self.#id.with_untracked(rt, |v| {
                        v.items()
                            .iter()
                            .find(|(k, _)| #sv::ToData::to_data(k) == *key)
                            .map(|(k, t)| (::core::clone::Clone::clone(k), #sv::ToData::to_data(t)))
                    })?;
                    let ::core::option::Option::Some((k, item)) = found else {
                        return ::core::result::Result::Err(#sv::core::Error::failed(
                            ::std::format!("`{}` holds no item with key {:?}", #fname, key),
                        ));
                    };
                    let new = item
                        .with_path(path, ::core::clone::Clone::clone(value))
                        .and_then(|d| <#item as #sv::FromData>::from_data(&d))
                        .map_err(|e| #sv::core::Error::failed(::std::format!("{}: {e}", #fname)))?;
                    self.#id.write_item_tagged(rt, k, new, move |rt, i, t, g| {
                        send(rt, i, #sv::ToData::to_data(t), g)
                    })?;
                    ::core::result::Result::Ok(())
                }));
                item_patches.push(quote!(#i => {
                    let reported = sent.iter().any(|p| match p {
                        #patch::#var(ds) => ds.iter().any(|d| match d {
                            #sv::core::VecDiff::Update { key: k, .. }
                            | #sv::core::VecDiff::Insert { key: k, .. } => #sv::ToData::to_data(k) == *key,
                            #sv::core::VecDiff::Reset { items } => {
                                items.iter().any(|(k, _)| #sv::ToData::to_data(k) == *key)
                            }
                            _ => false,
                        }),
                        _ => false,
                    });
                    if reported {
                        return ::core::option::Option::None;
                    }
                    self.#id
                        .iter()
                        .enumerate()
                        .find(|(_, t)| #sv::ToData::to_data(&#sv::Keyed::key(*t)) == *key)
                        .map(|(index, t)| #patch::#var(::std::vec![#sv::core::VecDiff::Update {
                            index,
                            key: #sv::Keyed::key(t),
                            value: ::core::clone::Clone::clone(t),
                        }]))
                }));
                snapshot.push(quote!(#id: self.#id.with_untracked(rt, |v| {
                    v.items().iter().map(|(_, t)| ::core::clone::Clone::clone(t)).collect()
                })?));
                reads.push(quote!(#i => self.#id.with(rt, |v| #sv::Data::List(
                    v.items().iter().map(|(_, t)| #sv::ToData::to_data(t)).collect()
                )),));
                ids.push(quote!(#i => ::std::vec![self.#id.id()],));
                writes.push(
                    quote!(#i => ::core::result::Result::Err(#sv::core::Error::failed(
                    ::std::format!("`{}` is a keyed list: it cannot be written", #fname)
                )),),
                );
                keyed_items.push(quote!(#i => self.#id.with_untracked(rt, |v| {
                    v.items().iter().map(|(_, t)| #sv::ToData::to_data(t)).collect()
                }),));
            }
            Kind::Event(payload) => {
                let i = ei;
                ei += 1;
                let (arity, args) = event_args(payload, &quote!(v));
                variants.push(quote!(#[doc = #doc] #var(#payload)));
                event_variants.push(quote!(#[doc = #doc] #var(#payload)));
                event_into.push(quote!(#event::#var(v) => #patch::#var(v),));
                cell_fields.push(quote!(pub #id: #sv::core::EventQueue<#payload>));
                cell_new.push(quote!(#id: rt.events::<#payload>()));
                event_infos.push(quote!(#sv::EventInfo {
                    name: #fname,
                    arity: #arity,
                    doc: #doc,
                }));
                applies_plain.push(quote!(#patch::#var(_) => {}));
                targets.push(quote!(#patch::#var(_) => #sv::Target::Event(#i),));
                cell_apply.push(quote!(#patch::#var(v) => {
                    self.#id.emit(rt, ::core::clone::Clone::clone(v))?;
                    ::core::result::Result::Ok(::core::option::Option::Some(#sv::Applied::Event {
                        event: #i,
                        args: #args,
                    }))
                }));
                snapshot.push(quote!(#id: ::core::default::Default::default()));
            }
        }
    }
    let patch_doc = format!("A change to [`{name}`]'s state, sent by its service thread.");
    let event_doc = format!("An event of [`{name}`], emitted with `Cx::emit`.");
    let cells_doc = format!(
        "[`{name}`]'s state on the logic thread: a core cell per field, a queue per event."
    );
    Ok(quote! {
        #[doc = #patch_doc]
        #[derive(Clone, Debug, PartialEq)]
        #vis enum #patch {
            #(#variants),*
        }

        #[doc = #event_doc]
        #[derive(Clone, Debug, PartialEq)]
        #vis enum #event {
            #(#event_variants),*
        }

        impl ::core::convert::From<#event> for #patch {
            fn from(e: #event) -> #patch {
                match e {
                    #(#event_into)*
                }
            }
        }

        #[doc = #cells_doc]
        #vis struct #cells {
            #(#cell_fields),*
        }

        impl ::core::fmt::Debug for #cells {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.debug_struct(::core::stringify!(#cells))
                    #(#debug)*
                    .finish()
            }
        }

        impl #sv::Patch for #patch {
            fn target(&self) -> #sv::Target {
                match self {
                    #(#targets)*
                }
            }
        }

        impl #sv::Store for #name {
            type Patch = #patch;
            type Event = #event;
            type Cells = #cells;
            const FIELDS: &'static [#sv::FieldInfo] = &[#(#infos),*];
            const EVENTS: &'static [#sv::EventInfo] = &[#(#event_infos),*];

            fn diff(old: &Self, new: &Self, out: &mut ::std::vec::Vec<#patch>) {
                #(#diffs)*
            }

            fn apply(&mut self, patch: &#patch) {
                match patch {
                    #(#applies_plain)*
                }
            }

            fn field_patch(&self, field: usize) -> ::core::option::Option<#patch> {
                match field {
                    #(#field_patches)*
                    _ => ::core::option::Option::None,
                }
            }

            #[allow(unreachable_patterns)]
            fn item_patch(
                &self,
                field: usize,
                key: &#sv::Data,
                sent: &[#patch],
            ) -> ::core::option::Option<#patch> {
                let _ = (&key, &sent);
                match field {
                    #(#item_patches)*
                    _ => ::core::option::Option::None,
                }
            }
        }

        impl #sv::Cells<#name> for #cells {
            fn new(rt: &#sv::core::Runtime, service: &str, initial: &#name) -> Self {
                #cells { #(#cell_new),* }
            }

            fn apply(
                &self,
                rt: &#sv::core::Runtime,
                patch: &#patch,
                how: #sv::How,
            ) -> ::core::result::Result<::core::option::Option<#sv::Applied>, #sv::core::Error> {
                match patch {
                    #(#cell_apply)*
                }
            }

            fn snapshot(&self, rt: &#sv::core::Runtime) -> ::core::result::Result<#name, #sv::core::Error> {
                ::core::result::Result::Ok(#name { #(#snapshot),* })
            }

            fn read(&self, rt: &#sv::core::Runtime, field: usize) -> ::core::result::Result<#sv::Data, #sv::core::Error> {
                match field {
                    #(#reads)*
                    _ => ::core::result::Result::Err(#sv::core::Error::failed(
                        ::std::format!("no field #{field}"),
                    )),
                }
            }

            fn ids(&self, field: usize) -> ::std::vec::Vec<#sv::core::NodeId> {
                match field {
                    #(#ids)*
                    _ => ::std::vec::Vec::new(),
                }
            }

            fn write(
                &self,
                rt: &#sv::core::Runtime,
                field: usize,
                value: &#sv::Data,
                send: #sv::SendWrite,
            ) -> ::core::result::Result<(), #sv::core::Error> {
                let _ = (&value, &send);
                match field {
                    #(#writes)*
                    _ => ::core::result::Result::Err(#sv::core::Error::failed(
                        ::std::format!("no field #{field}"),
                    )),
                }
            }

            fn write_item(
                &self,
                rt: &#sv::core::Runtime,
                field: usize,
                key: &#sv::Data,
                path: &[#sv::Step],
                value: &#sv::Data,
                send: #sv::SendItemWrite,
            ) -> ::core::result::Result<(), #sv::core::Error> {
                let _ = (&rt, &key, &path, &value, &send);
                match field {
                    #(#item_writes)*
                    _ => ::core::result::Result::Err(#sv::core::Error::failed(
                        ::std::format!("field #{field} is not a keyed list"),
                    )),
                }
            }

            fn forget_echoes(&self, rt: &#sv::core::Runtime) {
                let _ = rt;
                #(#forgets)*
            }

            fn keyed_items(
                &self,
                rt: &#sv::core::Runtime,
                field: usize,
            ) -> ::core::result::Result<::std::vec::Vec<#sv::Data>, #sv::core::Error> {
                match field {
                    #(#keyed_items)*
                    _ => ::core::result::Result::Err(#sv::core::Error::failed(
                        ::std::format!("field #{field} is not a keyed list"),
                    )),
                }
            }

            fn dispose(&self, rt: &#sv::core::Runtime) {
                #(#disposes)*
            }
        }
    })
}

#[derive(Default)]
struct DataAttr {
    name: Option<String>,
    key: Option<Ident>,
}

fn data_attr(attrs: &[Attribute]) -> syn::Result<DataAttr> {
    let mut out = DataAttr::default();
    for a in attrs {
        if !a.path().is_ident("data") {
            continue;
        }
        a.parse_nested_meta(|m| {
            if m.path.is_ident("name") {
                let s: LitStr = m.value()?.parse()?;
                out.name = Some(s.value());
                Ok(())
            } else if m.path.is_ident("key") {
                let id: Ident = m.value()?.parse()?;
                out.key = Some(id);
                Ok(())
            } else {
                Err(m.error("expected `name = \"…\"` or `key = field`"))
            }
        })?;
    }
    Ok(out)
}

fn rename(attrs: &[Attribute]) -> syn::Result<Option<String>> {
    let mut out = None;
    for a in attrs {
        if !a.path().is_ident("data") {
            continue;
        }
        a.parse_nested_meta(|m| {
            if m.path.is_ident("rename") {
                let s: LitStr = m.value()?.parse()?;
                out = Some(s.value());
                Ok(())
            } else {
                Err(m.error("expected `rename = \"…\"`"))
            }
        })?;
    }
    Ok(out)
}

/// `#[derive(Data)]`: a record struct (named fields, each `ToData +
/// FromData + SchemaType`) or a unit-only enum, converted to and from
/// `strand_services::Data` under its schema name (`#[data(name =
/// "Workspace")]`, default the type's name; enum variants in
/// snake_case). `#[data(key = id)]` on a struct implements `Keyed` by
/// that field; `#[data(rename = "type")]` on a field gives its schema
/// name.
#[proc_macro_derive(Data, attributes(data))]
pub fn derive_data(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match data(&input) {
        Ok(t) => t.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn data(input: &DeriveInput) -> syn::Result<Tokens> {
    let name = &input.ident;
    let sv = quote!(::strand_services);
    let attr = data_attr(&input.attrs)?;
    let schema = attr.name.clone().unwrap_or_else(|| name.to_string());
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new(
            input.generics.span(),
            "Data derives only on a type without generic parameters",
        ));
    }
    match &input.data {
        Data::Struct(s) => {
            let Fields::Named(fields) = &s.fields else {
                return Err(syn::Error::new(
                    input.span(),
                    "Data derives on a struct with named fields",
                ));
            };
            let mut to = Vec::new();
            let mut from = Vec::new();
            let mut key = None;
            for f in &fields.named {
                let Some(id) = &f.ident else { continue };
                let ty = &f.ty;
                let fname = rename(&f.attrs)?.unwrap_or_else(|| field_name(id));
                to.push(quote!((
                    ::std::borrow::Cow::Borrowed(#fname),
                    #sv::ToData::to_data(&self.#id)
                )));
                from.push(quote!(#id: <#ty as #sv::FromData>::from_data(
                    #sv::record_field(d, #schema, #fname)?
                ).map_err(|e| e.within(#fname))?));
                if attr.key.as_ref() == Some(id) {
                    key = Some(quote! {
                        impl #sv::Keyed for #name {
                            type Key = #ty;
                            const KEY_FIELD: &'static str = #fname;
                            fn key(&self) -> #ty {
                                ::core::clone::Clone::clone(&self.#id)
                            }
                        }
                    });
                }
            }
            if let (Some(k), None) = (&attr.key, &key) {
                return Err(syn::Error::new(k.span(), "the key names no field"));
            }
            Ok(quote! {
                impl #sv::ToData for #name {
                    fn to_data(&self) -> #sv::Data {
                        #sv::Data::Record {
                            ty: ::std::borrow::Cow::Borrowed(#schema),
                            fields: ::std::vec![#(#to),*],
                        }
                    }
                }

                impl #sv::FromData for #name {
                    fn from_data(d: &#sv::Data) -> ::core::result::Result<Self, #sv::DataError> {
                        ::core::result::Result::Ok(#name { #(#from),* })
                    }
                }

                impl #sv::SchemaType for #name {
                    fn schema_type() -> ::std::string::String {
                        ::std::string::String::from(#schema)
                    }
                }

                #key
            })
        }
        Data::Enum(e) => {
            let mut to = Vec::new();
            let mut from = Vec::new();
            for v in &e.variants {
                if !matches!(v.fields, Fields::Unit) {
                    return Err(syn::Error::new(
                        v.span(),
                        "Data derives on enums of unit variants only",
                    ));
                }
                let id = &v.ident;
                let vname = rename(&v.attrs)?.unwrap_or_else(|| snake(&id.to_string()));
                to.push(quote!(#name::#id => #vname,));
                from.push(quote!(#vname => ::core::result::Result::Ok(#name::#id),));
            }
            Ok(quote! {
                impl #sv::ToData for #name {
                    fn to_data(&self) -> #sv::Data {
                        let variant = match self { #(#to)* };
                        #sv::Data::Enum {
                            ty: ::std::borrow::Cow::Borrowed(#schema),
                            variant: ::std::borrow::Cow::Borrowed(variant),
                        }
                    }
                }

                impl #sv::FromData for #name {
                    fn from_data(d: &#sv::Data) -> ::core::result::Result<Self, #sv::DataError> {
                        match d {
                            #sv::Data::Enum { variant, .. } => match &**variant {
                                #(#from)*
                                other => ::core::result::Result::Err(#sv::DataError::new(
                                    ::std::format!("`{other}` is not a variant of {}", #schema),
                                )),
                            },
                            other => ::core::result::Result::Err(#sv::DataError::expected(#schema, other)),
                        }
                    }
                }

                impl #sv::SchemaType for #name {
                    fn schema_type() -> ::std::string::String {
                        ::std::string::String::from(#schema)
                    }
                }
            })
        }
        Data::Union(_) => Err(syn::Error::new(
            input.span(),
            "Data does not derive on unions",
        )),
    }
}

/// `#[derive(Call)]` on an enum: one variant per action or async method,
/// named in snake_case (`PlayPause` is `play_pause()`) or as
/// `#[call(name = "…")]` says. A variant's fields are the call's
/// arguments in order (each `FromData`), except a field named `item`,
/// which takes the item the action was called on (`ws.focus()`: the
/// `Workspace`). Variants of one name taking items of different records
/// (`item.activate()` and `entry.activate()`) are told apart by the
/// item's record type.
#[proc_macro_derive(Call, attributes(call))]
pub fn derive_call(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match call(&input) {
        Ok(t) => t.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn call(input: &DeriveInput) -> syn::Result<Tokens> {
    let name = &input.ident;
    let sv = quote!(::strand_services);
    let Data::Enum(e) = &input.data else {
        return Err(syn::Error::new(input.span(), "Call derives on an enum"));
    };
    let mut names = Vec::new();
    let mut arms = Vec::new();
    let mut items = Vec::new();
    let mut sigs = Vec::new();
    // Each variant's call name: `#[call(name = "…")]`, else snake_case.
    let mut cnames = Vec::new();
    for v in &e.variants {
        let mut cname = snake(&v.ident.to_string());
        for a in &v.attrs {
            if !a.path().is_ident("call") {
                continue;
            }
            a.parse_nested_meta(|m| {
                if m.path.is_ident("name") {
                    let lit: LitStr = m.value()?.parse()?;
                    cname = lit.value();
                    Ok(())
                } else {
                    Err(m.error("expected `name`"))
                }
            })?;
        }
        cnames.push(cname);
    }
    for (v, cname) in e.variants.iter().zip(&cnames) {
        let id = &v.ident;
        let cname = cname.clone();
        let shared = cnames.iter().filter(|c| **c == cname).count() > 1;
        if !names.contains(&cname) {
            names.push(cname.clone());
        }
        let mut n = 0usize;
        let mut item_of = None;
        let mut arg = |ty: &Type, fname: Option<&Ident>| -> Tokens {
            if fname.is_some_and(|f| f == "item") {
                items.push(quote!(<#ty as #sv::SchemaType>::schema_type()));
                item_of = Some(quote!(<#ty as #sv::SchemaType>::schema_type()));
                return quote!(<#ty as #sv::FromData>::from_data(
                    item.ok_or_else(|| #sv::DataError::new(::std::format!("`{}` needs an item", #cname)))?
                )?);
            }
            let i = n;
            n += 1;
            quote!(<#ty as #sv::FromData>::from_data(
                args.get(#i).unwrap_or(&#sv::Data::Null)
            ).map_err(|e| e.within(#cname))?)
        };
        let build = match &v.fields {
            Fields::Unit => quote!(#name::#id),
            Fields::Unnamed(f) => {
                let parts: Vec<_> = f.unnamed.iter().map(|f| arg(&f.ty, None)).collect();
                quote!(#name::#id(#(#parts),*))
            }
            Fields::Named(f) => {
                let parts: Vec<_> = f
                    .named
                    .iter()
                    .map(|f| {
                        let fid = f.ident.as_ref();
                        let a = arg(&f.ty, fid);
                        quote!(#fid: #a)
                    })
                    .collect();
                quote!(#name::#id { #(#parts),* })
            }
        };
        // A name several variants share: the item's record decides.
        let guard = match (&item_of, shared) {
            (Some(t), true) => quote!(if item.is_some_and(|d| matches!(
                d,
                #sv::Data::Record { ty, .. } if **ty == *#t
            ))),
            _ => quote!(),
        };
        arms.push(quote!(#cname #guard => ::core::result::Result::Ok(#build),));
        let item = item_of.map_or_else(
            || quote!(::core::option::Option::None),
            |t| quote!(::core::option::Option::Some(#t)),
        );
        sigs.push(quote!(#sv::CallSig { name: #cname, arity: #n, item: #item }));
    }
    Ok(quote! {
        impl #sv::FromCall for #name {
            const NAMES: &'static [&'static str] = &[#(#names),*];

            fn signatures() -> ::std::vec::Vec<#sv::CallSig> {
                ::std::vec![#(#sigs),*]
            }

            fn from_call(
                name: &str,
                item: ::core::option::Option<&#sv::Data>,
                args: &[#sv::Data],
            ) -> ::core::result::Result<Self, #sv::DataError> {
                let _ = (&item, &args);
                match name {
                    #(#arms)*
                    other => ::core::result::Result::Err(#sv::DataError::new(
                        ::std::format!("no call `{other}`"),
                    )),
                }
            }

            fn item_records() -> ::std::vec::Vec<::std::string::String> {
                ::std::vec![#(#items),*]
            }
        }
    })
}

#[derive(Default)]
struct ServiceAttr {
    name: Option<LitStr>,
    schema: Option<Expr>,
    action: Option<Type>,
    call: Option<Type>,
    fns: Option<Path>,
    thread: bool,
}

/// `#[service(name = "cpu")]` on a `Store` struct, its schema text the
/// `SCHEMA` constant in scope (or `schema = EXPR`):
/// implements `strand_services::Service`, running the struct's inherent
/// `async fn run(cx: Cx<Self>) -> Result<(), ServiceError>` on the shared
/// services runtime, or with `thread` its `fn run(cx: Cx<Self>) ->
/// Result<(), ServiceError>` on a thread of its own (for `!Send`
/// libraries: PipeWire, the Wayland toplevel protocols). Optional:
/// `action = T` and `call = T` (`#[derive(Call)]` enums of its actions
/// and async methods) and `fns = path` (its `fn` methods, computed on
/// the logic thread: `fn(&Cells, &Runtime, &str, &[Data]) ->
/// Option<Result<Data, Error>>`).
#[proc_macro_attribute]
pub fn service(args: TokenStream, item: TokenStream) -> TokenStream {
    let mut attr = ServiceAttr::default();
    let parser = syn::meta::parser(|m| {
        if m.path.is_ident("name") {
            attr.name = Some(m.value()?.parse()?);
        } else if m.path.is_ident("schema") {
            attr.schema = Some(m.value()?.parse()?);
        } else if m.path.is_ident("action") {
            attr.action = Some(m.value()?.parse()?);
        } else if m.path.is_ident("call") {
            attr.call = Some(m.value()?.parse()?);
        } else if m.path.is_ident("fns") {
            attr.fns = Some(m.value()?.parse()?);
        } else if m.path.is_ident("thread") {
            attr.thread = true;
        } else {
            return Err(m.error("expected `name`, `schema`, `action`, `call`, `fns` or `thread`"));
        }
        Ok(())
    });
    parse_macro_input!(args with parser);
    let item_tokens: Tokens = item.clone().into();
    let input = parse_macro_input!(item as DeriveInput);
    let ty = &input.ident;
    let sv = quote!(::strand_services);
    let Some(name) = attr.name else {
        return err(Span::call_site(), "#[service] needs `name = \"…\"`");
    };
    // Its schema text: `schema = …`, or the module's `SCHEMA` constant.
    let schema = attr.schema.map_or_else(|| quote!(SCHEMA), |e| quote!(#e));
    let action = attr
        .action
        .map_or_else(|| quote!(#sv::NoCall), |t| quote!(#t));
    let call = attr
        .call
        .map_or_else(|| quote!(#sv::NoCall), |t| quote!(#t));
    let fns = attr.fns.map(|p| {
        quote! {
            fn call(
                cells: &<Self as #sv::Store>::Cells,
                rt: &#sv::core::Runtime,
                method: &str,
                args: &[#sv::Data],
            ) -> ::core::option::Option<::core::result::Result<#sv::Data, #sv::core::Error>> {
                #p(cells, rt, method, args)
            }
        }
    });
    let start = if attr.thread {
        quote!(#sv::Start::thread(move || #ty::run(cx)))
    } else {
        quote!(#sv::Start::shared(move || #ty::run(cx)))
    };
    quote! {
        #item_tokens

        impl #sv::Service for #ty {
            const NAME: &'static str = #name;
            type Action = #action;
            type Call = #call;

            fn schema() -> &'static str {
                #schema
            }

            #fns

            fn start(cx: #sv::Cx<Self>) -> #sv::Start {
                #start
            }
        }
    }
    .into()
}
