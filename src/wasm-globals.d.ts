/**
 * The part of the WebAssembly JS API Sitefile uses. TypeScript ships these types
 * only with the DOM lib, which this Node project doesn't load.
 */
declare namespace WebAssembly {
  type ImportExportKind = 'function' | 'table' | 'memory' | 'global' | 'tag';
  interface ModuleImportDescriptor {
    readonly module: string;
    readonly name: string;
    readonly kind: ImportExportKind;
  }
  interface ModuleExportDescriptor {
    readonly name: string;
    readonly kind: ImportExportKind;
  }
  class Module {
    constructor(bytes: BufferSource);
    static imports(module: Module): ModuleImportDescriptor[];
    static exports(module: Module): ModuleExportDescriptor[];
  }
}
