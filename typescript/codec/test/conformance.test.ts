/**
 * The codec, checked against the cross-language conformance corpus.
 *
 * `conformance/element_type_vectors.json` is generated from `infrastore-core`'s
 * `codec::conformance` vectors and read by every binding's codec tests, so all
 * implementations are held to one definition of the encodings rather than to
 * each other.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import {
  type DecodedValues,
  decodeElementValues,
  ElementTypeError,
  parseElementType,
  physicalDtype,
} from "../src/index.ts";

interface ConformanceVector {
  name: string;
  element_type: string;
  time_series_type: string;
  leading_dims: number;
  shape: number[];
  values: number[];
  bytes_hex: string;
  decoded: { kind: string; timesteps?: unknown };
}

const VECTORS_URL = new URL(
  "../../../conformance/element_type_vectors.json",
  import.meta.url,
);

const vectors: ConformanceVector[] = JSON.parse(
  readFileSync(fileURLToPath(VECTORS_URL), "utf8"),
).vectors;

/** The vector's values as the little-endian bytes the store holds. */
function storedBytes(vector: ConformanceVector): Uint8Array {
  const values = Float64Array.from(vector.values);
  return new Uint8Array(values.buffer.slice(0));
}

function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

test("the corpus is non-empty", () => {
  assert.ok(vectors.length > 0, "no conformance vectors were loaded");
});

for (const vector of vectors) {
  test(`${vector.name} decodes to its pinned values`, () => {
    const bytes = storedBytes(vector);
    // The byte-level contract: the same values must encode to the same bytes.
    assert.equal(hex(bytes), vector.bytes_hex);

    const decoded = decodeElementValues(
      bytes,
      vector.shape,
      vector.element_type,
      vector.leading_dims,
    ) as DecodedValues;
    assert.ok(decoded !== null, "a composite element type must decode");
    assert.equal(decoded.kind, vector.decoded.kind);
    assert.deepEqual(decoded.timesteps, vector.decoded.timesteps);
  });
}

test("scalar and non-f64 arrays have nothing to decode", () => {
  const bytes = new Uint8Array(Float64Array.from([1, 2, 3]).buffer);
  assert.equal(decodeElementValues(bytes, [3], "f64"), null);
  const ints = new Uint8Array(Int32Array.from([1, 2, 3]).buffer);
  assert.equal(decodeElementValues(ints, [1, 3], "tuple(3,i32)"), null);
});

test("an array the element type does not describe is rejected", () => {
  const bytes = new Uint8Array(new Float64Array(8).buffer);
  assert.throws(
    () => decodeElementValues(bytes, [2, 4], "quadratic_function"),
    ElementTypeError,
  );
  assert.throws(
    () => decodeElementValues(bytes, [2, 4], "piecewise_linear"),
    ElementTypeError,
  );
});

test("element types round-trip through their canonical spelling", () => {
  const spellings = [
    "f64",
    "bool",
    "u8",
    "tuple(3,f64)",
    "tuple(4,i32)",
    "linear_function",
    "quadratic_function",
    "piecewise_linear",
    "piecewise_step",
  ];
  for (const spelling of spellings) {
    assert.doesNotThrow(() => parseElementType(spelling), spelling);
  }
  for (const bad of ["", "float64", "PiecewiseLinearData", "tuple(0,f64)", "tuple(3)"]) {
    assert.throws(() => parseElementType(bad), ElementTypeError, bad);
  }
  assert.equal(physicalDtype(parseElementType("piecewise_step")), "f64");
  assert.equal(physicalDtype(parseElementType("tuple(4,i32)")), "i32");
});

test("a misaligned byte view still decodes", () => {
  // A protobuf `bytes` field can land at any offset in a pooled buffer;
  // `Float64Array` cannot be constructed over a misaligned one, so the codec
  // copies. Pin that the copy path agrees with the aligned one.
  const values = Float64Array.from([2, 0, 1, 1, 3]);
  const aligned = new Uint8Array(values.buffer);
  const padded = new Uint8Array(aligned.length + 1);
  padded.set(aligned, 1);
  const misaligned = padded.subarray(1);

  assert.deepEqual(
    decodeElementValues(misaligned, [1, 5], "piecewise_linear"),
    decodeElementValues(aligned, [1, 5], "piecewise_linear"),
  );
});
