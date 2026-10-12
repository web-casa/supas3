// SPDX-License-Identifier: AGPL-3.0-or-later
//! Gives the Windows executable its icon and version details.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../packaging/icons/supas3.ico");
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("../../packaging/icons/supas3.ico")
            .set("ProductName", "supaS3")
            .set("FileDescription", "supaS3")
            .set("LegalCopyright", "supaS3 contributors. AGPL-3.0-or-later.");
        resource
            .compile()
            .expect("the Windows resources could not be compiled");
    }
}
