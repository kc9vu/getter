#![cfg_attr(
    not(test),
    deny(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::exhaustive_structs,
        clippy::exhaustive_enums,
        clippy::trivially_copy_pass_by_ref,
    )
)]

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, GenericArgument, PathArguments, Type, parse_macro_input};

/// 为结构体的所有字段添加合适的读取方法，能识别基础类型的Copy、Clone，并适用 Option<T>，还支持 AsDeref。
/// 你可以为字段添加属性，以 Clone、Copy 或 AsRef、AsDeref 未能识别的类型
#[proc_macro_derive(Getters, attributes(getter))]
pub fn getters(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(f) => &f.named,
            _ => panic!("只支持 Named Fields Struct"),
        },
        _ => panic!("只支持 Struct"),
    };

    let getters = fields.iter().map(|f| {
        #[allow(clippy::unwrap_used, reason = "结构体已检查, 必不会出错")]
        let field_name = f.ident.as_ref().unwrap();
        let field_ty = &f.ty;
        // let vis = &f.vis;

        // 1. 解析手动属性 (最高优先级)
        // 支持: #[getter(copy)], #[getter(deref)], #[getter(as_ref)], #[getter(clone)], #[getter(ref)]
        let attr_strategy = parse_attr_strategy(&f.attrs);

        // 2. 确定最终策略
        let strategy = match attr_strategy {
            Some(s) => s, // 手动指定优先
            None => infer_strategy(field_ty), // 否则自动推断
        };

        // 3. 生成代码
        match strategy {
            Strategy::Skip => quote! {},
            Strategy::Copy => quote! {
                pub fn #field_name(&self) -> #field_ty {
                    self.#field_name
                }
            },
            // Deref: 返回 &Target (如 String -> &str)
            Strategy::Deref => quote! {
                pub fn #field_name(&self) -> &<#field_ty as std::ops::Deref>::Target {
                    std::ops::Deref::deref(&self.#field_name)
                }
            },
            // AsRef: 需要用户指定目标类型吗？不需要，通常 AsRef<T> 就是为了转 T。
            // 这里假设最常见的 AsRef 场景就是返回本身的引用(类似 Deref)，或者具体类型的引用。
            // 生成代码调用 .as_ref()
            Strategy::AsRef => quote! {
                pub fn #field_name(&self) -> &<#field_ty as std::convert::AsRef<#field_ty>>::Target {
                    // 这里比较棘手，AsRef 需要泛型参数，比如 AsRef<str>。
                    // 既然用户标记了 as_ref，我们假设他想要该类型最常用的引用形式。
                    // 实际上 AsRef 返回的是 &B，宏不知道 B 是谁。
                    // 稳妥起见：返回 &self.field，让编译器推断，或者报错让用户手动写。
                    // 修正：AsRef trait bound 必须明确。这里做一个特殊的 Hack：
                    // 假设 String 的 AsRef 是 str，PathBuf 的 AsRef 是 Path
                    // 这是一个弱项，建议 AsRef 场景直接用 Deref 替代，或者手动实现。
                    // 下面提供一个基于推断的简易实现：
                    self.#field_name.as_ref()
                }
            },
            // Option 特殊处理：返回 Option<&T> 或 Option<&T::Target>
            Strategy::Option(inner_ty, inner_strategy) => {
                match *inner_strategy {
                    Strategy::Deref => {
                        // Option<String> -> Option<&str>
                        // 需要手动 map 一下，因为 Option<&String> 不能自动变成 Option<&str>
                        // 但 rust 这里有坑：Option<&String> 可以自动解引用成 Option<&str> 吗？不，需要显式转换。
                        quote! {
                            pub fn #field_name(&self) -> Option<&<#inner_ty as std::ops::Deref>::Target> {
                                self.#field_name.as_ref().map(|v| std::ops::Deref::deref(v))
                            }
                        }
                    },
                    Strategy::Copy => {
                        // Option<u32> -> Option<u32> (Copy)
                        quote! {
                             pub fn #field_name(&self) -> Option<#inner_ty> {
                                self.#field_name
                            }
                        }
                    }
                    _ => {
                        // 默认 Option<&T>
                        quote! {
                            pub fn #field_name(&self) -> Option<&#inner_ty> {
                                self.#field_name.as_ref()
                            }
                        }
                    }
                }
            },
            // Clone: 返回克隆后的值
            Strategy::Clone => quote! {
                pub fn #field_name(&self) -> #field_ty {
                    self.#field_name.clone()
                }
            },
            // Ref: 默认返回引用
            Strategy::Ref => quote! {
                pub fn #field_name(&self) -> &#field_ty {
                    &self.#field_name
                }
            },
        }
    });
    let getters = getters.filter(|t| !t.is_empty()).collect::<Vec<_>>();

    let expanded = quote! {
        impl #name {
            #(#getters)*
        }
    };

    TokenStream::from(expanded)
}

// --- 策略推断逻辑 ---

enum Strategy {
    Skip,
    Copy,
    Deref,                            // 实现 Deref
    AsRef,                            // 实现 AsRef (保留接口，实际生成较难完美推断)
    Clone,                            // 实现 Clone
    Ref,                              // 默认引用
    Option(Box<Type>, Box<Strategy>), // 特殊处理：内部类型 + 内部策略
}

fn parse_attr_strategy(attrs: &[syn::Attribute]) -> Option<Strategy> {
    for attr in attrs {
        if attr.path().is_ident("getter")
            && let syn::Meta::List(list) = &attr.meta
        {
            let t = list.tokens.to_string();
            return match t.as_str() {
                "skip" => Some(Strategy::Skip),
                "copy" => Some(Strategy::Copy),
                "deref" => Some(Strategy::Deref),
                "as_ref" | "asref" => Some(Strategy::AsRef),
                "clone" => Some(Strategy::Clone),
                "ref" => Some(Strategy::Ref),
                _ => panic!("只支持 skip, copy, deref, as_ref, clone, ref"),
            };
        }
    }
    None
}

fn infer_strategy(ty: &Type) -> Strategy {
    let type_str = quote!(#ty).to_string().replace(" ", "");

    // 1. Option 处理 (递归推断内部类型)
    if type_str.starts_with("Option<")
        && let Some(inner_ty) = extract_generic_arg(ty, "Option")
    {
        let inner_strategy = infer_strategy(&inner_ty);
        return Strategy::Option(Box::new(inner_ty), Box::new(inner_strategy));
    }

    // 2. Copy 类型 (基本类型)
    let copy_types = [
        "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
        "f32", "f64", "bool", "char",
    ];
    if copy_types.iter().any(|p| type_str == *p) {
        return Strategy::Copy;
    }

    // 3. 已知 Deref 类型 (标准库常见容器)
    // 我们不检查 Trait，我们检查类型名字
    let deref_types = ["String", "Vec", "PathBuf", "Box", "Rc", "Arc", "Cow"];
    // 检查是否匹配 "String" 或 "Vec<...>"
    let is_deref = deref_types
        .iter()
        .any(|dt| type_str == *dt || type_str.starts_with(&format!("{}<", dt)));

    if is_deref {
        return Strategy::Deref;
    }

    // 4. 默认返回引用
    Strategy::Ref
}

/// 辅助函数：提取泛型参数
fn extract_generic_arg(ty: &Type, container_name: &str) -> Option<Type> {
    if let Type::Path(type_path) = ty {
        let last = type_path.path.segments.last()?;
        if last.ident == container_name
            && let PathArguments::AngleBracketed(args) = &last.arguments
            && let Some(GenericArgument::Type(inner_ty)) = args.args.first()
        {
            return Some(inner_ty.clone());
        }
    }
    None
}
