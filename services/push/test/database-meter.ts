// Count workerd's real D1 row metadata while exercising the production handler.
// No production logging or instrumentation is needed for this cost regression.
export function meterDatabase(database:D1Database) {
 const totals={read:0,written:0};
 const originals=new WeakMap<D1PreparedStatement,D1PreparedStatement>();
 const record=(result:D1Result)=>{totals.read+=result.meta.rows_read;totals.written+=result.meta.rows_written;};
 function statement(original:D1PreparedStatement):D1PreparedStatement {
  const proxy=new Proxy(original,{get(target,key){
   if(key==="bind")return (...values:unknown[])=>statement(target.bind(...values));
   if(key==="first")return async(column?:string)=>{
    const result=await target.all<Record<string,unknown>>();record(result);
    return column===undefined?result.results[0]??null:result.results[0]?.[column]??null;
   };
   if(key==="run" || key==="all")return async()=>{const result=await target[key]();record(result);return result;};
   const value=Reflect.get(target,key);return typeof value==="function"?value.bind(target):value;
  }});
  originals.set(proxy,original);return proxy;
 }
 const db=new Proxy(database,{get(target,key){
  if(key==="prepare")return (query:string)=>statement(target.prepare(query));
  if(key==="batch")return async(statements:D1PreparedStatement[])=>{
   const results=await target.batch(statements.map(s=>originals.get(s)??s));results.forEach(record);return results;
  };
  const value=Reflect.get(target,key);return typeof value==="function"?value.bind(target):value;
 }});
 return {db,totals};
}
