//! `#[model]`: one struct is both a row of a Mattermost table and the JSON the API sends, with Go's rules.
//!
//! ```ignore
//! #[model(table = "users")]
//! pub struct User {
//!     pub id: String,
//!     #[mm(omitempty)]
//!     pub create_at: i64,              // column `createat`, JSON `create_at`, left out of JSON when 0
//!     #[mm(transient, omitempty)]
//!     pub last_activity_at: i64,       // JSON only, not a column
//!     #[mm(skip_json)]
//!     pub mfa_secret: String,          // column only (Go `json:"-"`)
//! }
//! ```
//!
//! - Column name: the field name without underscores (`create_at` → `createat`), or `#[mm(column = "...")]`.
//! - JSON name: the field name (`r#type` → `type`), or `#[mm(json = "...")]`.
//! - JSON in: a missing field or `null` gives the zero value, unknown fields are ignored, as Go's decoder does.
//! - Row in: a NULL column gives the zero value, as Mattermost's own NULL-free writes would.
//! - Generates `Default`, `Serialize`, `Deserialize`, `sqlx::FromRow`, and with a table, `crate::db::Table`
//!   (`SELECT`, `INSERT`, `UPDATE` by `id`, and binding the columns in order).

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Field, Fields, ItemStruct, LitStr, Token, Type, parse::Parser, parse_macro_input, punctuated::Punctuated,
    spanned::Spanned,
};

#[derive(Default)]
struct FieldOpts {
    omitempty: bool,
    transient: bool,
    skip_json: bool,
    column: Option<String>,
    json: Option<String>,
    pgtype: Option<String>,
}

fn field_opts(f: &mut Field) -> syn::Result<FieldOpts> {
    let mut o = FieldOpts::default();
    let mut err = None;
    f.attrs.retain(|a| {
        if !a.path().is_ident("mm") {
            return true;
        }
        let r = a.parse_nested_meta(|m| {
            if m.path.is_ident("omitempty") {
                o.omitempty = true;
            } else if m.path.is_ident("transient") {
                o.transient = true;
            } else if m.path.is_ident("skip_json") {
                o.skip_json = true;
            } else if m.path.is_ident("column") {
                o.column = Some(m.value()?.parse::<LitStr>()?.value());
            } else if m.path.is_ident("json") {
                o.json = Some(m.value()?.parse::<LitStr>()?.value());
            } else if m.path.is_ident("pgtype") {
                o.pgtype = Some(m.value()?.parse::<LitStr>()?.value());
            } else {
                return Err(m.error("expected omitempty, transient, skip_json, column, json or pgtype"));
            }
            Ok(())
        });
        if let Err(e) = r {
            err = Some(e);
        }
        false
    });
    err.map_or(Ok(o), Err)
}

fn is_option(t: &Type) -> bool {
    matches!(t, Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "Option"))
}

#[proc_macro_attribute]
pub fn model(args: TokenStream, input: TokenStream) -> TokenStream {
    let mut item = parse_macro_input!(input as ItemStruct);
    let mut table = None;
    let parser = syn::meta::parser(|m| {
        if m.path.is_ident("table") {
            table = Some(m.value()?.parse::<LitStr>()?.value());
            Ok(())
        } else {
            Err(m.error("expected table = \"...\""))
        }
    });
    if let Err(e) = parser.parse(args) {
        return e.to_compile_error().into();
    }
    match expand(&mut item, table) {
        Ok(t) => t.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(item: &mut ItemStruct, table: Option<String>) -> syn::Result<proc_macro2::TokenStream> {
    let Fields::Named(fields) = &mut item.fields else {
        return Err(syn::Error::new(item.span(), "#[model] needs named fields"));
    };
    let mut columns = vec![]; // (column, ident, pgtype)
    let mut row_fields = vec![];
    for f in fields.named.iter_mut() {
        let o = field_opts(f)?;
        let ident = f.ident.clone().unwrap();
        let name = ident.to_string().trim_start_matches("r#").to_string();
        let json = o.json.unwrap_or_else(|| name.clone());
        let mut serde: Punctuated<proc_macro2::TokenStream, Token![,]> = Punctuated::new();
        if o.skip_json {
            serde.push(quote!(skip));
        } else {
            serde.push(quote!(rename = #json));
            serde.push(quote!(deserialize_with = "crate::model::null_default"));
            if o.omitempty {
                serde.push(quote!(skip_serializing_if = "crate::model::is_zero"));
            }
        }
        f.attrs.push(syn::parse_quote!(#[serde(#serde)]));
        if o.transient {
            row_fields.push(quote!(#ident: ::core::default::Default::default()));
        } else {
            let col = o.column.unwrap_or_else(|| name.replace('_', ""));
            let get = if is_option(&f.ty) {
                quote!(::sqlx::Row::try_get(row, #col)?)
            } else {
                quote!(crate::db::col(row, #col)?)
            };
            row_fields.push(quote!(#ident: #get));
            columns.push((col, ident, o.pgtype.unwrap_or_default()));
        }
    }

    let name = &item.ident;
    let (impl_g, ty_g, where_g) = item.generics.split_for_impl();
    let from_row = quote! {
        impl<'r> ::sqlx::FromRow<'r, ::sqlx::postgres::PgRow> for #name #ty_g #where_g {
            fn from_row(row: &'r ::sqlx::postgres::PgRow) -> ::sqlx::Result<Self> {
                Ok(Self { #(#row_fields),* })
            }
        }
    };
    let table_impl = table.map(|t| {
        let cols: Vec<&str> = columns.iter().map(|(c, _, _)| c.as_str()).collect();
        let types: Vec<&str> = columns.iter().map(|(_, _, p)| p.as_str()).collect();
        // A Postgres enum column (`Channels.Type`) is read as text and written with a cast.
        let sel: Vec<String> = columns
            .iter()
            .map(|(c, _, p)| if p.is_empty() { c.clone() } else { format!("{c}::text AS {c}") })
            .collect();
        let param = |i: usize| {
            let p = &columns[i].2;
            if p.is_empty() { format!("${}", i + 1) } else { format!("${}::{p}", i + 1) }
        };
        let select = format!("SELECT {} FROM {t}", sel.join(", "));
        let params: Vec<String> = (0..cols.len()).map(param).collect();
        let insert = format!("INSERT INTO {t} ({}) VALUES ({})", cols.join(", "), params.join(", "));
        // UPDATE binds the columns in the same order as INSERT, so `id` must come first.
        let update = if cols.first() == Some(&"id") {
            let sets: Vec<String> =
                cols.iter().enumerate().skip(1).map(|(i, c)| format!("{c} = {}", param(i))).collect();
            format!("UPDATE {t} SET {} WHERE id = $1", sets.join(", "))
        } else {
            String::new()
        };
        let idents = columns.iter().map(|(_, i, _)| i);
        quote! {
            impl #impl_g crate::db::Table for #name #ty_g #where_g {
                const TABLE: &'static str = #t;
                const COLUMNS: &'static [&'static str] = &[#(#cols),*];
                const PGTYPES: &'static [&'static str] = &[#(#types),*];
                const SELECT: &'static str = #select;
                const INSERT: &'static str = #insert;
                const UPDATE: &'static str = #update;
                fn bind<'q>(&'q self, q: crate::db::Query<'q>) -> crate::db::Query<'q> {
                    q #(.bind(&self.#idents))*
                }
            }
        }
    });

    Ok(quote! {
        #[derive(::core::default::Default, ::serde::Serialize, ::serde::Deserialize)]
        #[serde(default)]
        #item
        #from_row
        #table_impl
    })
}
