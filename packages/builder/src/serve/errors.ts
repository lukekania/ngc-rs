export interface ParsedDiagnostic {
  file: string | null;
  line: number | null;
  column: number | null;
  message: string;
}

const ANSI_RE = /\u001b\[[0-9;]*m/g;

export function stripAnsi(s: string): string {
  return s.replace(ANSI_RE, '');
}

export function parseDiagnostics(stderrChunk: string): ParsedDiagnostic[] {
  const cleaned = stripAnsi(stderrChunk);
  const lines = cleaned.split(/\r?\n/);
  const diagnostics: ParsedDiagnostic[] = [];
  for (const line of lines) {
    const diag = parseLine(line);
    if (diag) {
      diagnostics.push(diag);
    }
  }
  return diagnostics;
}

const FILE_LINE_COL = /^\s*(?:Error:?\s*)?([\/\.\w][^\s:]*\.\w+):(\d+):(\d+)\s*[-:]?\s*(.*)$/;
const ERROR_PREFIX = /^\s*Error:\s+(.+)$/i;

function parseLine(line: string): ParsedDiagnostic | null {
  const trimmed = line.trim();
  if (!trimmed) {
    return null;
  }
  const fileMatch = FILE_LINE_COL.exec(trimmed);
  if (fileMatch) {
    const lineNo = Number(fileMatch[2]);
    const colNo = Number(fileMatch[3]);
    return {
      file: fileMatch[1] ?? null,
      line: Number.isFinite(lineNo) ? lineNo : null,
      column: Number.isFinite(colNo) ? colNo : null,
      message: (fileMatch[4] ?? '').trim() || trimmed,
    };
  }
  const errMatch = ERROR_PREFIX.exec(trimmed);
  if (errMatch) {
    return { file: null, line: null, column: null, message: errMatch[1] ?? trimmed };
  }
  return null;
}

export function summarizeDiagnostics(diagnostics: ParsedDiagnostic[]): string {
  if (diagnostics.length === 0) {
    return 'ngc-rs reported a build failure (no parseable diagnostics in output).';
  }
  return diagnostics
    .map((d) => {
      if (d.file && d.line != null && d.column != null) {
        return `${d.file}:${d.line}:${d.column} ${d.message}`.trim();
      }
      if (d.file) {
        return `${d.file} ${d.message}`.trim();
      }
      return d.message;
    })
    .join('\n');
}
