use super::*;
use crate::ocr_tests::Fixture;
use gptgrep_parse::validate_ocr_result;
use std::{os::unix::fs::PermissionsExt, time::SystemTime};

fn executor(fixture: &Fixture, mode: &str) -> (OcrHostExecutor, OcrExecutionContext, PathBuf) {
    let root = fixture.dir.path().canonicalize().unwrap();
    let script = root.join("native-task-replay");
    let code = r#"#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys, time
MODE = '__MODE__'
def send(value):
    print(json.dumps(value, separators=(',', ':')), flush=True)
for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    if method == 'initialize':
        send({'id':message['id'],'result':{}})
    elif method == 'account/read':
        send({'id':message['id'],'result':{'account':{'type':'chatgpt'}}})
    elif method == 'config/read':
        send({'id':message['id'],'result':{'config':{'mcp_servers':{}}}})
    elif method == 'thread/start':
        config = message['params']['config']
        assert config['features.shell_tool'] is True and config['features.view_image'] is True
        assert 'features.code_mode.enabled' not in config and 'web_search' not in config
        assert message['params']['dynamicTools'] == []
        send({'id':message['id'],'result':{'thread':{'id':'native-thread'},'model':'gpt-6-luna','modelProvider':'openai','reasoningEffort':'max','serviceTier':'priority','approvalPolicy':'never','activePermissionProfile':{'id':'native-ocr-test'},'sandbox':{'type':'workspaceWrite','writableRoots':[],'networkAccess':False}}})
    elif method == 'turn/start':
        state = json.loads(message['params']['input'][0]['text'])
        task = state['task']
        send({'id':message['id'],'result':{'turn':{'id':'native-turn','status':'inProgress'}}})
        if MODE == 'hang':
            child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
            pending = pathlib.Path(__file__ + '.pids.pending')
            pending.write_text(json.dumps([os.getpid(), child.pid]))
            pending.rename(pathlib.Path(__file__ + '.pids'))
            time.sleep(60)
        if MODE == 'source_changed':
            pathlib.Path(state['source_path']).write_bytes(b'changed source')
        status = 'partial' if MODE == 'partial' else 'complete'
        result = {'schema_version':'gptgrep.ocr.result.v1','task_id':task['id'],'task_sha256':task['task_sha256'],'source_sha256':task['source_sha256'],'prepared_sha256':task['prepared_sha256'],'page':task['page'],'status':status,'regions':[{'id':region['id'],'bbox':region['bbox'],'text':'synthetic native transcription','reading_order':index,'status':status} for index,region in enumerate(task['regions'])]}
        items = [{'type':'commandExecution','id':'native-code','status':'completed'}, {'type':'imageView','id':'native-image','path':state['images'][0]['path']}, {'type':'agentMessage','id':'answer','phase':'final_answer','text':json.dumps(result)}]
        send({'method':'thread/tokenUsage/updated','params':{'threadId':'native-thread','turnId':'native-turn','tokenUsage':{'total':{'inputTokens':7,'outputTokens':3}}}})
        send({'method':'turn/completed','params':{'threadId':'native-thread','turn':{'id':'native-turn','status':'completed','error':None,'items':items}}})
"#.replace("__MODE__", mode);
    fs::write(&script, &code).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.join("empty-codex-home");
    let scratch = root.join("scratch");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&scratch).unwrap();
    let runtime_profile = OcrRuntimeProfile::with_native_agent(
        "a".repeat(40),
        "b".repeat(64),
        hash(code.as_bytes()),
        NativeAgentRuntimeProfile {
            permission_profile: "native-ocr-test".into(),
            scratch_root: scratch,
            capabilities: BTreeMap::new(),
            native_skills: Vec::new(),
            native_runtime_package: None,
            max_tool_calls: 96,
            timeout_secs: 1200,
        },
    )
    .unwrap();
    assert!(!runtime_profile.image_qualification_accepted());
    assert!(runtime_profile.execution_ready());
    let context = OcrExecutionContext::new(
        "native-ocr-replay",
        &"c".repeat(64),
        SystemTime::now() + Duration::from_secs(10),
        4,
    )
    .unwrap();
    let executor = OcrHostExecutor::new(
        OcrHostConfig {
            codex_bin: script.to_string_lossy().into_owned(),
            codex_home: home,
            codex_state_dir: None,
            prepared_root: root.clone(),
            raw_results_dir: root.join("raw"),
            runtime_profile,
            transport: OcrImageTransport::NativeAgent,
        },
        context.clone(),
    )
    .unwrap();
    (
        executor,
        context,
        script.with_file_name("native-task-replay.pids"),
    )
}

#[tokio::test]
async fn native_agent_replay_produces_source_bound_typed_result_without_legacy_qualification() {
    let fixture = Fixture::new();
    let (executor, context, _) = executor(&fixture, "complete");
    let result = executor.execute(fixture.task.clone()).await.unwrap();
    validate_ocr_result(&fixture.task, &result).unwrap();
    assert!(result.receipt.image_deliveries.is_empty());
    let native = result.receipt.native_agent.as_ref().unwrap();
    assert_eq!(native.tool_calls, 2);
    assert_eq!(native.tool_budget, 96);
    assert_eq!(native.image_views, 1);
    assert_eq!(context.snapshot().unwrap().terminal_admissions, 1);
    let retained = fs::read(
        fixture
            .dir
            .path()
            .join("raw")
            .join(format!("ocr-{}.json", native.execution_sha256)),
    )
    .unwrap();
    assert_eq!(hash(&retained), native.execution_sha256);
    let report: crate::NativeTaskReport = serde_json::from_slice(&retained).unwrap();
    assert_eq!(
        report.permission_observation.as_ref().unwrap()["permission_profile"],
        "native-ocr-test"
    );
    let mut forged = result.clone();
    forged.receipt.native_agent.as_mut().unwrap().task_sha256 = "d".repeat(64);
    assert!(validate_ocr_result_metadata(&fixture.task, &forged).is_err());
    forged = result.clone();
    forged.regions.clear();
    assert!(validate_ocr_result_metadata(&fixture.task, &forged).is_err());
}

#[tokio::test]
async fn native_agent_replay_rejects_changed_sources_and_incomplete_regions() {
    for mode in ["source_changed", "partial"] {
        let fixture = Fixture::new();
        let (executor, context, _) = executor(&fixture, mode);
        assert!(executor.execute(fixture.task).await.is_err(), "{mode}");
        assert_eq!(context.snapshot().unwrap().terminal_admissions, 1);
    }
}

#[tokio::test]
async fn native_agent_cancellation_reaps_the_owned_process_group() {
    let fixture = Fixture::new();
    let (executor, context, pids) = executor(&fixture, "hang");
    let cancel = OcrCancellation::default();
    let worker_cancel = cancel.clone();
    let worker = tokio::spawn(async move {
        executor
            .execute_agent_page(fixture.task, Some(worker_cancel))
            .await
    });
    tokio::time::timeout(Duration::from_secs(4), async {
        while !pids.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let owned: Vec<i32> = serde_json::from_slice(&fs::read(&pids).unwrap()).unwrap();
    cancel.cancel();
    let error = worker.await.unwrap().err().unwrap();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    assert_eq!(context.snapshot().unwrap().quarantined_slots, 1);
    for pid in owned {
        // SAFETY: signal 0 only checks the liveness of the replay's recorded owned process.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
}
