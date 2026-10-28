// Application execution-only client. This module is never a model tool or UI
// bundle. It owns no credential database and never returns plaintext to callers.
import {createDecipheriv,createHash,timingSafeEqual} from 'node:crypto';
import {readFile} from 'node:fs/promises';
import {readFileSync} from 'node:fs';

export class CredentialDeliveryError extends Error {
  constructor(){super('credential_delivery_unavailable');}
}
const deny=()=>{throw new CredentialDeliveryError();};
export const canonical=value=>JSON.stringify(value,(_k,v)=>v&&typeof v==='object'&&!Array.isArray(v)?
  Object.fromEntries(Object.keys(v).sort().map(k=>[k,v[k]])):v);
export const digest=value=>createHash('sha256').update(canonical(value)).digest('hex');
export class CredentialClient {
  #url;#token;#consumer;#keyFile;#materialType;#handlingMode;
  constructor({url,token,consumer,keyFile,materialType,handlingMode}){
    const u=new URL(url);
    if(u.username||u.password||u.search||u.hash||u.pathname!=='/'||
      !(u.protocol==='https:'||u.protocol==='http:'&&['localhost','127.0.0.1','[::1]'].includes(u.hostname))||!token||!consumer||!keyFile)deny();
    if(!materialType||!['platform_managed','application_managed'].includes(handlingMode))deny();
    this.#url=u.origin;this.#token=token;this.#consumer=consumer;this.#keyFile=keyFile;this.#materialType=materialType;this.#handlingMode=handlingMode;
  }
  async #call(path,body){
    try{
      const r=await fetch(this.#url+'/v1/config/application-credentials/'+path,{method:'POST',redirect:'error',
        headers:{Authorization:'Bearer '+this.#token,'Content-Type':'application/json'},body:JSON.stringify(body),signal:AbortSignal.timeout(15000),cache:'no-store'});
      if(!r.ok)deny();return await r.json();
    }catch{deny();}
  }
  async enter(credentialKey,fields,expectedRevision){
    const r=await this.#call('enter',{consumer_id:this.#consumer,credential_key:credentialKey,material_type:this.#materialType,fields,
      ...(expectedRevision?{expected_revision:expectedRevision}:{})});
    if(typeof r.credential_ref?.id!=='string'||!Number.isSafeInteger(r.credential_ref.revision))deny();
    return r.credential_ref;
  }
  request(binding){
    const keys=JSON.parse(readFileSync(this.#keyFile,'utf8')).keys;if(keys?.length!==1)deny();
    if(!binding.execution)deny();
    return {consumer_id:this.#consumer,credential_key:binding.account_ref,material_type:this.#materialType,
      expected_revision:binding.credential.revision,operation_id:binding.operation_id,
      runtime_epoch:binding.epoch,plan_hash:binding.plan_hash,target:binding.target,
      recipient:keys[0].holder.trust_domain,recipient_key_id:keys[0].id,execution:binding.execution};
  }
  async resolveExecution(credentialKey,selector){
    if(!selector||typeof selector.run_id!=='string'||!selector.run_id||selector.session_id!=null&&typeof selector.session_id!=='string')deny();
    const r=await this.#call('execution',{consumer_id:this.#consumer,credential_key:credentialKey,...(selector.session_id?{session_id:selector.session_id}:{}),run_id:selector.run_id});
    if(r.handling_mode!==this.#handlingMode||!r.execution?.session_id||selector.session_id&&r.execution.session_id!==selector.session_id||selector.agent_id&&r.execution?.agent_id!==selector.agent_id||r.execution?.run_id!==selector.run_id)deny();
    return Object.freeze(r.execution);
  }
  async assertCurrent(binding){
    const r=await this.#call('check',this.request(binding));if(r.current!==true)deny();
  }
  async withMaterial(binding,consume){
    let plaintext,material,consumerEntered=false;
    try{
      const request=this.request(binding),r=await this.#call('deliver',request);
      const keyDoc=JSON.parse(await readFile(this.#keyFile,'utf8'));
      const envelope=r.envelope,a=r.access,h=r.holder,b=r.binding;
      if(envelope?.type!=='sealed_for_workload'||h?.boundary!=='workload'||envelope.recipient!==h.trust_domain||
        envelope.expires_at_unix_ms<=Date.now()||envelope.expires_at_unix_ms>Date.now()+121000||
        a?.credential?.id!==binding.credential.id||a.credential.revision!==binding.credential.revision||
        (a.policy?.handling_mode??'platform_managed')!==this.#handlingMode||a.policy?.model_exposure!=='forbidden'||canonical(a.policy.allowed_plaintext_holders)!==canonical([h])||
        a.target?.purpose?.type!=='extension'||a.target.audience!==`application://${this.#consumer}/${binding.account_ref}`||
        a.usage?.consumer_id!==this.#consumer||a.usage.material_type!==this.#materialType)deny();
      const prefix='awaken-recipient:v1:',ref=envelope.envelope_ref?.id;
      if(typeof ref!=='string'||!ref.startsWith(prefix)||ref.length>100000)deny();
      const sealed=JSON.parse(Buffer.from(ref.slice(prefix.length),'base64').toString('utf8'));
      const keys=keyDoc.keys.filter(k=>k.id===sealed.key_id&&canonical(k.holder)===canonical(h));if(keys.length!==1)deny();
      const holder={boundary:h.boundary,trust_domain:h.trust_domain};
      const target={purpose:{type:a.target.purpose.type},audience:a.target.audience};
      const usage={type:'extension',consumer_id:a.usage.consumer_id,material_type:a.usage.material_type,public_config:a.usage.public_config};
      const policy={...(this.#handlingMode==='application_managed'?{handling_mode:this.#handlingMode}:{}),allowed_plaintext_holders:[holder],model_exposure:'forbidden'};
      const aad=Buffer.from(JSON.stringify([prefix,{id:a.credential.id,revision:a.credential.revision},a.material_source,
        target,usage,policy,holder,{workspace_id:b.workspace_id,target_use_fingerprint:b.target_use_fingerprint},sealed.key_id,envelope.expires_at_unix_ms]));
      const ciphertext=Buffer.from(sealed.ciphertext),nonce=Buffer.from(sealed.nonce),key=Buffer.from(keys[0].key_base64,'base64');
      if(nonce.length!==12||key.length!==32||ciphertext.length<17)deny();
      try{
        const decipher=createDecipheriv('chacha20-poly1305',key,nonce,{authTagLength:16});
        decipher.setAAD(aad);decipher.setAuthTag(ciphertext.subarray(-16));
        plaintext=Buffer.concat([decipher.update(ciphertext.subarray(0,-16)),decipher.final()]);
      }finally{key.fill(0);}
      const payload=JSON.parse(plaintext.toString('utf8'));
      if(canonical(payload.context)!==canonical(request)||typeof payload.material!=='string'||
        !payload.material.startsWith('awaken-credential-material-v1:'))deny();
      material=JSON.parse(payload.material.slice('awaken-credential-material-v1:'.length));
      if(material.type_id!==this.#materialType||!material.fields||Object.keys(material.fields).length>16||Object.values(material.fields).some(v=>typeof v!=='string'))deny();
      await this.assertCurrent(binding);
      consumerEntered=true;
      await consume(Object.freeze({useField:async(field,write)=>{
        if(!Object.hasOwn(material.fields,field)||typeof material.fields[field]!=='string')deny();
        if(envelope.expires_at_unix_ms<=Date.now())deny();
        await this.assertCurrent(binding);
        if(envelope.expires_at_unix_ms<=Date.now())deny();
        await write(material.fields[field]);
      }}));
    }catch(e){if(consumerEntered)throw e;throw new CredentialDeliveryError();}
    finally{plaintext?.fill(0);if(material?.fields)for(const field of Object.keys(material.fields))material.fields[field]='';}
  }
}
