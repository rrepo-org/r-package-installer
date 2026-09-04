# Windows native-code fixtures

`x86_64-windows.dll` is a minimal linked PE32+ DLL exporting a function that
returns 42. `x86_64-windows.obj` is the corresponding relocatable COFF object.
They were generated from this source:

```c
__declspec(dllexport) int answer(void) {
    return 42;
}
```

The source was compiled with Clang for `x86_64-pc-windows-msvc`, then linked
with LLVM's `lld-link` using `/dll /noentry /machine:x64 /export:answer`.
