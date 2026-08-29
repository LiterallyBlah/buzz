import assert from "node:assert/strict";
import { test } from "node:test";

import { moduleDataUrl } from "./moduleDataUrl.mjs";

test("moduleDataUrl carries stub source without a custom load hook", async () => {
  const loaded = await import(
    moduleDataUrl("export const loaderContract = 'sourceful';\n")
  );

  assert.equal(loaded.loaderContract, "sourceful");
});
