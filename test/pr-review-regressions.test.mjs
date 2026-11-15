import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

const styles = await readFile(
  new URL("../web/src/styles.css", import.meta.url),
  "utf8",
);

test("view tabs expose a visible keyboard focus indicator", () => {
  assert.match(
    styles,
    /button:focus-visible,[\s\S]*?\[tabindex\][^{]*\{[^}]*outline:\s*2px solid color-mix\(in srgb, var\(--accent\) 70%, transparent\);[^}]*outline-offset:\s*1px;/s,
  );
  assert.doesNotMatch(styles, /\.view-tab:focus-visible\s*\{[^}]*outline:\s*0;/s);
});
