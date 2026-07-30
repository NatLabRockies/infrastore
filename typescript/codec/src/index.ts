/**
 * Decode infrastore arrays into per-timestep values.
 *
 * The gRPC read surface ships raw bytes: `value_bytes` plus a `shape` and an
 * `element_type` string. That keeps payloads compact and the server simple, and
 * leaves exactly one decode implementation per language. This is that
 * implementation for TypeScript.
 *
 * The encodings are specified in the store's
 * `docs/src/reference/element-types.md`, and pinned by
 * `conformance/element_type_vectors.json` at the repo root, which this package's
 * tests read.
 */

/** Physical element widths a stored array can hold. */
export type Dtype =
  | "f64"
  | "f32"
  | "i64"
  | "i32"
  | "i16"
  | "i8"
  | "u64"
  | "u32"
  | "u16"
  | "u8"
  | "bool";

const DTYPES: ReadonlySet<string> = new Set<Dtype>([
  "f64",
  "f32",
  "i64",
  "i32",
  "i16",
  "i8",
  "u64",
  "u32",
  "u16",
  "u8",
  "bool",
]);

/** What a stored array's elements mean. */
export type ElementType =
  | { readonly kind: "scalar"; readonly dtype: Dtype }
  | { readonly kind: "tuple"; readonly arity: number; readonly dtype: Dtype }
  | { readonly kind: "linear_function" }
  | { readonly kind: "quadratic_function" }
  | { readonly kind: "piecewise_linear" }
  | { readonly kind: "piecewise_step" };

/** One `(x, y)` point of a piecewise-linear curve — directly plottable. */
export interface XyPoint {
  readonly x: number;
  readonly y: number;
}

/** `f(x) = proportional * x + constant`. */
export interface LinearFunction {
  readonly proportional: number;
  readonly constant: number;
}

/** `f(x) = quadratic * x^2 + proportional * x + constant`. */
export interface QuadraticFunction {
  readonly quadratic: number;
  readonly proportional: number;
  readonly constant: number;
}

/** A step function: `n` x-coordinates and the `n - 1` y-values between them. */
export interface StepFunction {
  readonly x: number[];
  readonly y: number[];
}

/**
 * One decoded value per timestep, in row-major order over the array's leading
 * axes. The variant follows the element type's kind.
 */
export type DecodedValues =
  | { readonly kind: "tuple"; readonly timesteps: number[][] }
  | { readonly kind: "linear_function"; readonly timesteps: LinearFunction[] }
  | {
    readonly kind: "quadratic_function";
    readonly timesteps: QuadraticFunction[];
  }
  | { readonly kind: "piecewise_linear"; readonly timesteps: XyPoint[][] }
  | { readonly kind: "piecewise_step"; readonly timesteps: StepFunction[] };

/** Thrown when an element type is unspellable or contradicts its array. */
export class ElementTypeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ElementTypeError";
  }
}

const TUPLE_PATTERN = /^tuple\((\d+),([a-z0-9]+)\)$/;

/** Parse the canonical string form. Throws `ElementTypeError` if unknown. */
export function parseElementType(spelling: string): ElementType {
  if (DTYPES.has(spelling)) {
    return { kind: "scalar", dtype: spelling as Dtype };
  }
  switch (spelling) {
    case "linear_function":
    case "quadratic_function":
    case "piecewise_linear":
    case "piecewise_step":
      return { kind: spelling };
  }
  const tuple = TUPLE_PATTERN.exec(spelling);
  if (tuple) {
    const arity = Number(tuple[1]);
    const dtype = tuple[2];
    if (arity >= 1 && DTYPES.has(dtype)) {
      return { kind: "tuple", arity, dtype: dtype as Dtype };
    }
  }
  throw new ElementTypeError(
    `unknown element_type ${JSON.stringify(spelling)}; expected a dtype, ` +
      `tuple(N,dtype), linear_function, quadratic_function, piecewise_linear, ` +
      `or piecewise_step`,
  );
}

/** The physical dtype the stored bytes are encoded in. */
export function physicalDtype(elementType: ElementType): Dtype {
  switch (elementType.kind) {
    case "scalar":
    case "tuple":
      return elementType.dtype;
    default:
      return "f64";
  }
}

/** Whether one timestep occupies a variable number of the row's slots. */
export function isRagged(elementType: ElementType): boolean {
  return elementType.kind === "piecewise_linear" ||
    elementType.kind === "piecewise_step";
}

/**
 * Decode a stored array into its per-timestep values.
 *
 * `bytes` is the response's `value_bytes`, `shape` its `shape`, `elementType`
 * its `element_type` string, and `leadingDims` how many leading axes precede the
 * per-step element shape: 1 for a static series, 2 for a `Deterministic`, 3 for
 * a `Probabilistic` or `Scenarios`.
 *
 * Returns `null` when there is nothing to decode — a scalar element type, or any
 * array whose physical dtype is not `f64`. There the stored elements already are
 * the values, so the caller reads them straight out of `bytes` with the
 * appropriate typed-array view.
 */
export function decodeElementValues(
  bytes: Uint8Array,
  shape: readonly number[],
  elementType: string | ElementType,
  leadingDims = 1,
): DecodedValues | null {
  const type = typeof elementType === "string"
    ? parseElementType(elementType)
    : elementType;
  if (physicalDtype(type) !== "f64" || type.kind === "scalar") return null;
  if (shape.length < leadingDims) {
    throw new ElementTypeError(
      `shape [${shape}] has fewer than the ${leadingDims} leading dims its ` +
        `time-series type requires`,
    );
  }
  const elementDims = shape.slice(leadingDims);
  const width = expectedWidth(type, elementDims);
  const values = f64View(bytes);
  const rows = width === 0 ? 0 : values.length / width;
  if (!Number.isInteger(rows)) {
    throw new ElementTypeError(
      `${values.length} values do not divide into rows of width ${width}`,
    );
  }

  switch (type.kind) {
    case "tuple":
      return {
        kind: "tuple",
        timesteps: eachRow(values, width, rows, (row) => Array.from(row)),
      };
    case "linear_function":
      return {
        kind: "linear_function",
        timesteps: eachRow(values, width, rows, (row) => ({
          proportional: row[0],
          constant: row[1],
        })),
      };
    case "quadratic_function":
      return {
        kind: "quadratic_function",
        timesteps: eachRow(values, width, rows, (row) => ({
          quadratic: row[0],
          proportional: row[1],
          constant: row[2],
        })),
      };
    case "piecewise_linear":
      return {
        kind: "piecewise_linear",
        timesteps: eachRow(values, width, rows, (row, index) => {
          const n = leadingCount(row, index, width, 1 + 2 * countOf(row));
          const points: XyPoint[] = [];
          for (let k = 0; k < n; k++) {
            points.push({ x: row[1 + 2 * k], y: row[2 + 2 * k] });
          }
          return points;
        }),
      };
    case "piecewise_step":
      return {
        kind: "piecewise_step",
        timesteps: eachRow(values, width, rows, (row, index) => {
          const n = countOf(row);
          // `n` x-coords then `n - 1` y-values, so the row's used span is `2n`
          // — except for an empty timestep, whose only slot is the count.
          leadingCount(row, index, width, n === 0 ? 1 : 2 * n);
          return {
            x: Array.from(row.subarray(1, 1 + n)),
            y: Array.from(row.subarray(1 + n, n === 0 ? 1 : 2 * n)),
          };
        }),
      };
  }
}

/** The row width `elementType` requires of `elementDims`, validating it. */
function expectedWidth(
  elementType: ElementType,
  elementDims: readonly number[],
): number {
  const fixed = fixedWidth(elementType);
  if (fixed !== null) {
    if (elementDims.length !== 1 || elementDims[0] !== fixed) {
      throw new ElementTypeError(
        `element_type ${elementType.kind} requires per-step element dims ` +
          `[${fixed}], got [${elementDims}]`,
      );
    }
    return fixed;
  }
  if (elementDims.length !== 1) {
    throw new ElementTypeError(
      `element_type ${elementType.kind} requires exactly one per-step element ` +
        `dim (the row width), got [${elementDims}]`,
    );
  }
  const width = elementDims[0];
  const ok = elementType.kind === "piecewise_linear"
    ? width % 2 === 1
    : width === 1 || width % 2 === 0;
  if (width < 1 || !ok) {
    throw new ElementTypeError(
      `element_type ${elementType.kind} cannot have row width ${width}`,
    );
  }
  return width;
}

function fixedWidth(elementType: ElementType): number | null {
  switch (elementType.kind) {
    case "tuple":
      return elementType.arity;
    case "linear_function":
      return 2;
    case "quadratic_function":
      return 3;
    default:
      return null;
  }
}

function countOf(row: Float64Array): number {
  return row[0];
}

/** Validate a ragged row's leading count and return it. */
function leadingCount(
  row: Float64Array,
  index: number,
  width: number,
  needed: number,
): number {
  const raw = row[0];
  if (!Number.isInteger(raw) || raw < 0) {
    throw new ElementTypeError(
      `row ${index} leading count is ${raw}, which is not a non-negative ` +
        `whole number`,
    );
  }
  if (needed > width) {
    throw new ElementTypeError(
      `row ${index} declares ${raw} points, which needs ${needed} slots but ` +
        `the row width is ${width}`,
    );
  }
  return raw;
}

function eachRow<T>(
  values: Float64Array,
  width: number,
  rows: number,
  decode: (row: Float64Array, index: number) => T,
): T[] {
  const out: T[] = new Array(rows);
  for (let i = 0; i < rows; i++) {
    out[i] = decode(values.subarray(i * width, (i + 1) * width), i);
  }
  return out;
}

/**
 * A little-endian `Float64Array` over `bytes`. A zero-copy view when the buffer
 * is 8-byte aligned (the common case for a protobuf `bytes` field), else a copy
 * — `Float64Array` cannot be constructed over a misaligned offset.
 *
 * Reading `bytes` as native-endian is correct on every platform Node and
 * browsers run on today; a big-endian host would need a `DataView` loop, which
 * would cost the zero-copy path for a case that does not exist in practice.
 */
function f64View(bytes: Uint8Array): Float64Array {
  if (bytes.byteOffset % 8 === 0) {
    return new Float64Array(
      bytes.buffer,
      bytes.byteOffset,
      bytes.byteLength / 8,
    );
  }
  return new Float64Array(bytes.slice().buffer);
}
