import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const interfaces=["rust-sdk","typescript-sdk","mcp","playwright","puppeteer"] as const;
const proofDir=await mkdtemp(join(tmpdir(),"interface-proof-matrix-"));
try {
  const tests=interfaces.map(name=>`dist/test/${name}.test.js`);
  const child=spawn(process.execPath,["--test",...tests],{stdio:["ignore","pipe","pipe"],env:{...process.env,CONFORMANCE_PROOF_DIR:proofDir}});
  let output="";child.stdout.setEncoding("utf8");child.stderr.setEncoding("utf8");child.stdout.on("data",c=>{output=(output+String(c)).slice(-32768)});child.stderr.on("data",c=>{output=(output+String(c)).slice(-32768)});
  const[code]=await once(child,"exit") as [number|null,NodeJS.Signals|null];assert.equal(code,0,output);
  const records=await Promise.all(interfaces.map(async name=>JSON.parse(await readFile(join(proofDir,`${name}.json`),"utf8")) as {proof:unknown;rawEvidence:Array<{kind:string;sha256:string;size:number}>;normalization:string}));
  for(let index=1;index<records.length;index++)assert.deepEqual(records[index]!.proof,records[0]!.proof,`${interfaces[index]} weakened the canonical normalized proof`);
  for(const record of records){assert.match(record.normalization,/raw sha256 and size verified/);assert.deepEqual(record.rawEvidence.map(item=>item.kind),["navigation","upload","screenshot","download"]);for(const item of record.rawEvidence){assert.match(item.sha256,/^[a-f0-9]{64}$/);assert(Number.isSafeInteger(item.size)&&item.size>0);}}
  for(const kind of ["upload","download"]){const exact=records.map(record=>record.rawEvidence.find(item=>item.kind===kind));for(let index=1;index<exact.length;index++)assert.deepEqual(exact[index],exact[0],`${kind} raw evidence must be byte-identical across interfaces`);}
  process.stdout.write(`${output}\n5-interface normalized proof equality: PASS\n`);
} finally { await rm(proofDir,{recursive:true,force:true}); }
