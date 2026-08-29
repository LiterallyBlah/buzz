import { fileURLToPath, pathToFileURL } from "node:url";
import fs from "node:fs";
import path from "node:path";
import ts from "typescript";

const srcRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "src",
);

const repoRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);

// `nextResolve` requires specifiers to be URLs or relative paths. Passing an
// absolute filesystem path happens to work on POSIX (node coerces it), but on
// Windows an absolute path like `C:\...` parses as a URL with protocol `c:`
// and every test run dies with ERR_UNSUPPORTED_ESM_URL_SCHEME. Hand absolute
// paths to node as proper file:// URLs on all platforms.
function toFileSpecifier(candidatePath) {
  return path.isAbsolute(candidatePath)
    ? pathToFileURL(candidatePath).href
    : candidatePath;
}

function resolveSourcePath(basePath) {
  // Existence decides, not path.extname — a dotted basename like
  // `ProfileAvatarEditor.utils` (→ .utils.ts on disk) looks like an
  // extension but still needs resolving.
  if (fs.existsSync(basePath) && fs.statSync(basePath).isFile()) {
    return basePath;
  }

  for (const extension of [".ts", ".tsx", ".js", ".jsx", ".mjs"]) {
    const candidate = `${basePath}${extension}`;
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }

  for (const extension of [".ts", ".tsx", ".js", ".jsx", ".mjs"]) {
    const candidate = path.join(basePath, `index${extension}`);
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }

  return null;
}

// emoji-mart ships a bundled CJS main that node's cjs-module-lexer cannot
// extract named exports from (`import { init } from "emoji-mart"` throws
// under node ESM even though the bundler handles it). Tests never exercise
// the picker, so serve inert stubs for the emoji-mart entrypoints.
const stubModules = new Map([
  [
    "emoji-mart",
    "export const init = (...args) => globalThis.__BUZZ_TEST_EMOJI_MART_INIT__?.(...args);\n" +
      "export const SearchIndex = { search: async () => [] };\n" +
      "export default {};\n",
  ],
  ["@emoji-mart/react", "export default function Picker() { return null; }\n"],
]);

const STUB_URL_PREFIX = "buzz-test-stub:";

// The real ForumComposer mounts TipTap, which never releases its jsdom
// handles and hangs the node:test process, so tests that render the projects
// panels need it stubbed. The stub lives here rather than in the test file
// because a `module.registerHooks` sync hook in front of this async chain
// makes Node 22 reject every uncached CommonJS module loaded after it
// registers (the chain's `source: null` answer fails the sync validator);
// upstream CI runs Node 24, where the interop works, and never sees it.
// Scoped by importer so anything outside the projects feature — including a
// future test of the composer itself — still gets the real component.
const FORUM_COMPOSER_SPECIFIER = "@/features/forum/ui/ForumComposer";
const FORUM_COMPOSER_STUB_URL = "buzz-test-stub-scoped:ForumComposer";
const FORUM_COMPOSER_STUB_SOURCE =
  "globalThis.__FORUM_COMPOSER_STUBBED__ = true;\n" +
  "export function ForumComposer() { return null; }\n";

// Vite resolves asset imports (`./logo.png`, `./logo.png?inline`) to a URL or
// base64 string at bundle time; node's ESM resolver has no such loader and
// throws on the query suffix. Serve an inert string so components that embed
// assets stay unit-testable.
const ASSET_SPECIFIER = /\.(?:png|jpe?g|gif|svg|webp|avif|ico)(?:\?[^/]*)?$/;
const ASSET_URL_PREFIX = "buzz-test-asset:";

export function resolve(specifier, context, nextResolve) {
  if (ASSET_SPECIFIER.test(specifier)) {
    return {
      shortCircuit: true,
      url: `${ASSET_URL_PREFIX}${specifier}`,
    };
  }
  if (stubModules.has(specifier)) {
    return {
      shortCircuit: true,
      url: `${STUB_URL_PREFIX}${specifier}`,
    };
  }
  if (
    specifier === FORUM_COMPOSER_SPECIFIER &&
    context.parentURL?.includes("/src/features/projects/")
  ) {
    return { shortCircuit: true, url: FORUM_COMPOSER_STUB_URL };
  }
  if (specifier === "@features-manifest") {
    const resolved = path.join(repoRoot, "preview-features.json");
    return nextResolve(toFileSpecifier(resolved), context);
  }
  if (specifier === "@model-capabilities-manifest") {
    const resolved = path.join(repoRoot, "scripts", "model-capabilities.json");
    return nextResolve(toFileSpecifier(resolved), context);
  }
  if (specifier.startsWith("@/")) {
    const stripped = specifier.slice(2);
    // Preserve explicit extensions (.mjs, .js, .json, .ts, etc.). The bundler
    // tolerates extensionless `@/` imports for source files; node's ESM
    // resolver does not, so resolve against the extensions the app uses.
    // Otherwise paths like `@/.../foo.mjs` would be coerced into `foo.mjs.ts`
    // and fail to resolve.
    const resolved = resolveSourcePath(`${srcRoot}/${stripped}`);
    return nextResolve(
      toFileSpecifier(resolved ?? `${srcRoot}/${stripped}`),
      context,
    );
  }
  // Resolve extensionless relative TS imports (e.g. `./parseImeta`) — the app's
  // bundler adds the extension, but node's ESM resolver does not. Without this,
  // any .ts that relative-imports a sibling .ts can't be imported from a test,
  // which previously forced stale inlined copies of the source under test.
  // Dotted basenames (`./ProfileAvatarEditor.utils`) look like extensions to
  // path.extname, so resolveSourcePath existence-checks instead.
  if (
    (specifier.startsWith("./") || specifier.startsWith("../")) &&
    context.parentURL?.startsWith("file:")
  ) {
    const parentPath = fileURLToPath(context.parentURL);
    const resolved = resolveSourcePath(
      path.resolve(path.dirname(parentPath), specifier),
    );
    if (resolved) {
      return nextResolve(toFileSpecifier(resolved), context);
    }
    return nextResolve(specifier, context);
  }
  return nextResolve(specifier, context);
}

export async function load(url, context, nextLoad) {
  if (url.startsWith(ASSET_URL_PREFIX)) {
    return {
      format: "module",
      shortCircuit: true,
      source: 'export default "test-asset";\n',
    };
  }

  if (url.startsWith(STUB_URL_PREFIX)) {
    return {
      format: "module",
      shortCircuit: true,
      source: stubModules.get(url.slice(STUB_URL_PREFIX.length)) ?? "",
    };
  }

  if (url === FORUM_COMPOSER_STUB_URL) {
    return {
      format: "module",
      shortCircuit: true,
      source: FORUM_COMPOSER_STUB_SOURCE,
    };
  }

  // The app bundler loads .json imports without attributes (e.g. the bare
  // `@emoji-mart/data` entrypoint); node's ESM resolver requires
  // `with { type: "json" }` on every hop. Serve json here so transitive
  // imports from source under test don't need bundler-only semantics.
  if (url.endsWith(".json")) {
    return {
      format: "json",
      shortCircuit: true,
      source: fs.readFileSync(fileURLToPath(url), "utf8"),
    };
  }

  // Vite handles side-effect CSS imports (e.g. `import "./card-texture.css"`
  // in shared/ui) at bundle time; node's ESM loader has no CSS support. Serve
  // them as empty modules so components with style imports stay unit-testable.
  if (url.endsWith(".css")) {
    return {
      format: "module",
      shortCircuit: true,
      source: "",
    };
  }

  if (url.endsWith(".tsx")) {
    const source = fs.readFileSync(fileURLToPath(url), "utf8");
    const transpiled = ts.transpileModule(source, {
      compilerOptions: {
        jsx: ts.JsxEmit.ReactJSX,
        module: ts.ModuleKind.ESNext,
        target: ts.ScriptTarget.ES2020,
      },
      fileName: fileURLToPath(url),
    });

    return {
      format: "module",
      shortCircuit: true,
      source: transpiled.outputText,
    };
  }

  return nextLoad(url, context);
}
