// Shared psql process edge for physical PostgreSQL test observations/injection.
// The product owns its schema and transactions. This module neither migrates
// nor emulates an authority. URL credentials remain in the child environment,
// and failures expose no command line, URL, SQL body or material.
import {spawnSync} from 'node:child_process';
import {pathToFileURL} from 'node:url';

export function postgresQuery({container, database, url}, statement) {
  const env={...process.env};
  if (!container) {
    const parsed=new URL(url);
    if(!['postgres:','postgresql:'].includes(parsed.protocol)) throw Error('invalid PostgreSQL probe configuration');
    Object.assign(env,{PGHOST:parsed.hostname,PGPORT:parsed.port||'5432',
      PGUSER:decodeURIComponent(parsed.username),PGPASSWORD:decodeURIComponent(parsed.password),
      PGDATABASE:decodeURIComponent(parsed.pathname.slice(1))});
  }
  const args=container ? ['exec','-i',container,'psql','-U','postgres','-d',database] : [];
  args.push('-X','-q','-A','-t','-v','ON_ERROR_STOP=1');
  const result=spawnSync(container ? 'docker' : 'psql',args,{env,input:statement,
    encoding:'utf8',timeout:30_000,maxBuffer:32*1024*1024});
  if(result.error || result.status!==0) throw Error(`PostgreSQL test query failed (status=${result.status}; signal=${result.signal ?? 'none'}; code=${result.error?.code ?? 'none'})`);
  return result.stdout.trim();
}

export function postgresRows(connection, statement, ...parameters) {
  let index=0;
  // Parameters here are exact non-secret test IDs. Quote them as SQL strings;
  // never interpolate a shell command or infer a second database coordinate.
  const query=statement.replace(/\?/g,()=>{
    if(index>=parameters.length) throw Error('missing PostgreSQL test parameter');
    return "'"+String(parameters[index++]).replaceAll("'","''")+"'";
  });
  if(index!==parameters.length) throw Error('extra PostgreSQL test parameter');
  const result=postgresQuery(connection,`BEGIN READ ONLY; SELECT row_to_json(observation) FROM (${query}) observation; COMMIT;`);
  return result ? result.split('\n').map(line=>JSON.parse(line)) : [];
}

// Python's existing business oracle uses the same read-only probe, rather than
// introducing another psql runner or copying FileStore data into a fake SQLite DB.
if (process.argv[1] && import.meta.url===pathToFileURL(process.argv[1]).href) {
  const {readFileSync}=await import('node:fs');
  const {connection,statement,parameters}=JSON.parse(readFileSync(0,'utf8'));
  console.log(JSON.stringify(postgresRows(connection,statement,...parameters)));
}
