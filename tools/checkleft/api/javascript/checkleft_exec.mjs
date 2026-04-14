import fs from "node:fs";

export function readRequest() {
  const input = fs.readFileSync(process.stdin.fd, "utf8");
  return JSON.parse(input);
}

export function writeResponse(findings) {
  process.stdout.write(JSON.stringify({ findings: Array.from(findings) }));
}
