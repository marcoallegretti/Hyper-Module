import assert from "node:assert/strict";

import { parseTree, printParseErrorCode } from "jsonc-parser";

export function strictParse(text) {
  const errors = [];
  const tree = parseTree(text, errors, {
    allowEmptyContent: false,
    allowTrailingComma: false,
    disallowComments: true,
  });
  assert(
    tree && errors.length === 0,
    `invalid JSON: ${errors
      .map((error) => printParseErrorCode(error.error))
      .join(", ")}`,
  );
  assertNoDuplicateNames(tree, "$");
  const value = JSON.parse(text);
  assertIJson(value, "$");
  return value;
}

export function assertIJson(value, location) {
  if (typeof value === "number") {
    assert(Number.isFinite(value), `${location}: number must be finite`);
    assert(!Object.is(value, -0), `${location}: negative zero is forbidden`);
    if (Number.isInteger(value)) {
      assert(
        Number.isSafeInteger(value),
        `${location}: integer must be exactly interoperable`,
      );
    }
    return;
  }

  if (typeof value === "string") {
    for (let index = 0; index < value.length; index += 1) {
      const code = value.charCodeAt(index);
      if (code >= 0xd800 && code <= 0xdbff) {
        const next = value.charCodeAt(index + 1);
        assert(
          next >= 0xdc00 && next <= 0xdfff,
          `${location}: unpaired high surrogate`,
        );
        index += 1;
      } else {
        assert(
          code < 0xdc00 || code > 0xdfff,
          `${location}: unpaired low surrogate`,
        );
      }
    }
    return;
  }

  if (Array.isArray(value)) {
    value.forEach((item, index) => assertIJson(item, `${location}[${index}]`));
    return;
  }

  if (value !== null && typeof value === "object") {
    for (const [key, item] of Object.entries(value)) {
      assertIJson(key, `${location} key`);
      assertIJson(item, `${location}.${key}`);
    }
  }
}

function assertNoDuplicateNames(node, location) {
  if (node.type === "object") {
    const names = new Set();
    for (const property of node.children ?? []) {
      const [nameNode, valueNode] = property.children;
      assert(
        !names.has(nameNode.value),
        `${location}: duplicate object name ${nameNode.value}`,
      );
      names.add(nameNode.value);
      assertNoDuplicateNames(valueNode, `${location}.${nameNode.value}`);
    }
  } else if (node.type === "array") {
    for (const [index, child] of (node.children ?? []).entries()) {
      assertNoDuplicateNames(child, `${location}[${index}]`);
    }
  }
}
