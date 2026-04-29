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
const NGC_REBUILD_FAILED = /^\s*ngc-rs (?:rebuild|build) failed:\s*(.+)$/i;
const FILE_PATH_INLINE = /(?:in\s+)?(\/[\w./-]+\.\w+|[A-Za-z]:[\w./\\-]+\.\w+)(?::(\d+)(?::(\d+))?)?/;

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
  const ngcMatch = NGC_REBUILD_FAILED.exec(trimmed);
  if (ngcMatch && ngcMatch[1]) {
    const detail = ngcMatch[1];
    const inline = FILE_PATH_INLINE.exec(detail);
    if (inline) {
      const lineNo = inline[2] ? Number(inline[2]) : null;
      const colNo = inline[3] ? Number(inline[3]) : null;
      return {
        file: inline[1] ?? null,
        line: lineNo !== null && Number.isFinite(lineNo) ? lineNo : null,
        column: colNo !== null && Number.isFinite(colNo) ? colNo : null,
        message: detail,
      };
    }
    return { file: null, line: null, column: null, message: detail };
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
