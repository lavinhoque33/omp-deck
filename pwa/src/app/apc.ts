// Drops APC strings (`ESC _ … ESC \`) from a session's terminal bytes before xterm sees them.
// omp's native-UI frames (TSP, `ESC _ tsp;…`) and kitty graphics travel as APC strings; xterm
// renders neither, and it logs a parsing error for every non-ASCII character inside one (its
// parser has no transition for them in the APC state), so a TSP frame holding "…" filled the
// console. The stream is UTF-8: only the 7-bit introducer and terminator count (a bare 0x9c
// byte is a continuation byte, never ST). As in ECMA-48, CAN/SUB abort the string and any other
// ESC ends it and starts a real escape sequence.

const ESC = 0x1b;
const APC = 0x5f; // '_'
const ST_FINAL = 0x5c; // '\'
const CAN = 0x18;
const SUB = 0x1a;

const enum State {
  Ground,
  /** An ESC outside a string, held until the next byte says whether it opens an APC. */
  Escape,
  Apc,
  /** An ESC inside an APC: `\` ends the string, anything else starts an escape sequence. */
  ApcEscape,
}

export class ApcStripper {
  #state = State.Ground;

  /** `bytes` without the APC strings in them; state carries over to the next call. */
  filter(bytes: Uint8Array): Uint8Array {
    const out = new Uint8Array(bytes.length + 1);
    let n = 0;
    for (let i = 0; i < bytes.length; i++) {
      const b = bytes[i]!;
      switch (this.#state) {
        case State.Ground:
          if (b === ESC) this.#state = State.Escape;
          else out[n++] = b;
          break;
        case State.Escape:
          if (b === APC) {
            this.#state = State.Apc;
          } else {
            out[n++] = ESC;
            if (b === ESC) break; // a second ESC is held in turn
            out[n++] = b;
            this.#state = State.Ground;
          }
          break;
        case State.Apc:
          if (b === ESC) this.#state = State.ApcEscape;
          else if (b === CAN || b === SUB) this.#state = State.Ground;
          break;
        case State.ApcEscape:
          if (b === ST_FINAL) {
            this.#state = State.Ground;
          } else if (b === APC) {
            this.#state = State.Apc; // the string ended and another one began
          } else if (b === ESC) {
            this.#state = State.Escape;
          } else {
            out[n++] = ESC;
            out[n++] = b;
            this.#state = State.Ground;
          }
          break;
      }
    }
    return out.subarray(0, n);
  }
}
