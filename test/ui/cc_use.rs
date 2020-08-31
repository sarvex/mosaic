cc_use!("cc_use.h", A, Zed, B);
cc_use!("cc_use.h" in libfoo, B);
cc_use!("cc_use.h" in "libfoo", B, Foo::C, Foo::D, Bar::E);
cc_use!(<cc_use.h>, A);
cc_use!("cc_use.h", in "libfoo", B);
cc_use!("cc_use.h"; B);
cc_use!("cc_use.h" on "libfoo", B);
cc_use!("nonexistent.h" in "libfoo", B);

// TODO the top two should be found.
cc_use!("cc_use.h" in "libfoo", Templated<::i32>);
cc_use!("cc_use.h" in "libfoo", Templated<i32>);
cc_use!("cc_use.h" in "libfoo", Templated<'a>);
cc_use!("cc_use.h" in "libfoo", Templated<&i32>);
cc_use!("cc_use.h" in "libfoo", Templated<<Foo as Deref>::Target>);
