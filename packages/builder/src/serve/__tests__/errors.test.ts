import { describe, expect, it } from 'vitest';

import { parseDiagnostics, stripAnsi, summarizeDiagnostics } from '../errors';

describe('parseDiagnostics', () => {
  it('extracts file/line/column from a typical compiler line', () => {
    const out = parseDiagnostics(
      'src/app/hello.component.ts:12:5 - error TS2304: Cannot find name "Foo".',
    );
    expect(out).toHaveLength(1);
    const first = out[0];
    expect(first).toBeDefined();
    expect(first?.file).toBe('src/app/hello.component.ts');
    expect(first?.line).toBe(12);
    expect(first?.column).toBe(5);
    expect(first?.message).toContain('Cannot find name');
  });

  it('strips ANSI color codes before parsing', () => {
    const ansi = '\u001b[31msrc/a.ts:3:1 -\u001b[0m parse error here';
    const out = parseDiagnostics(ansi);
    expect(out).toHaveLength(1);
    const first = out[0];
    expect(first).toBeDefined();
    expect(first?.file).toBe('src/a.ts');
    expect(first?.line).toBe(3);
  });

  it('captures bare Error: lines without coordinates', () => {
    const out = parseDiagnostics('Error: failed to resolve module "x"');
    expect(out).toHaveLength(1);
    const first = out[0];
    expect(first).toBeDefined();
    expect(first?.file).toBeNull();
    expect(first?.message).toContain('failed to resolve');
  });

  it('extracts the file path from ngc-rs rebuild-failed lines', () => {
    const out = parseDiagnostics(
      'ngc-rs rebuild failed: template compile error in /a/b/app.ts: parser panicked: Unterminated string',
    );
    expect(out).toHaveLength(1);
    expect(out[0]?.file).toBe('/a/b/app.ts');
    expect(out[0]?.message).toContain('Unterminated string');
  });

  it('captures line/col when present in rebuild-failed messages', () => {
    const out = parseDiagnostics(
      'ngc-rs rebuild failed: error in /a/b/app.ts:42:7 unexpected token',
    );
    expect(out).toHaveLength(1);
    expect(out[0]?.file).toBe('/a/b/app.ts');
    expect(out[0]?.line).toBe(42);
    expect(out[0]?.column).toBe(7);
  });

  it('returns nothing for purely informational output', () => {
    expect(parseDiagnostics('ngc-rs build complete 12 modules')).toHaveLength(0);
  });
});

describe('summarizeDiagnostics', () => {
  it('formats a multi-diagnostic block with file:line:col prefixes', () => {
    const summary = summarizeDiagnostics([
      { file: 'a.ts', line: 1, column: 2, message: 'oops' },
      { file: null, line: null, column: null, message: 'bare msg' },
    ]);
    expect(summary).toContain('a.ts:1:2 oops');
    expect(summary).toContain('bare msg');
  });

  it('returns a fallback string when no diagnostics were parsed', () => {
    expect(summarizeDiagnostics([])).toMatch(/no parseable diagnostics/);
  });
});

describe('stripAnsi', () => {
  it('removes color escapes', () => {
    expect(stripAnsi('\u001b[1mhi\u001b[0m')).toBe('hi');
  });
});
