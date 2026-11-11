fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/kimono.ico");
        res.set("ProductName", "Kimono");
        res.set("FileDescription", "Kimono");
        let _ = res.compile();
    }
}
