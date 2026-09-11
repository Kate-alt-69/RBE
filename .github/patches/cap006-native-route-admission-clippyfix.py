from pathlib import Path

path = Path("engine/crates/route-engine/src/discovery.rs")
text = path.read_text(encoding="utf-8")

old_execute = '''async fn execute(
    inline_file: Arc<ModuleFile>,
    module_program: Arc<ModuleProgram>,
    native_plan: Option<Arc<NativeRoutePlan>>,
    takes_request: bool,
    state: AppState,
    params: HashMap<String, String>,
    query: HashMap<String, String>,
    request: Request,
) -> Response {
'''
new_execute = '''#[derive(Clone)]
struct RouteHandlerPlan {
    inline_file: Arc<ModuleFile>,
    module_program: Arc<ModuleProgram>,
    native_plan: Option<Arc<NativeRoutePlan>>,
    takes_request: bool,
}

async fn execute(
    plan: RouteHandlerPlan,
    state: AppState,
    params: HashMap<String, String>,
    query: HashMap<String, String>,
    request: Request,
) -> Response {
    let RouteHandlerPlan {
        inline_file,
        module_program,
        native_plan,
        takes_request,
    } = plan;
'''
if text.count(old_execute) != 1:
    raise SystemExit(f"CAP-006 execute anchor changed: {text.count(old_execute)}")
text = text.replace(old_execute, new_execute, 1)

old_plan = '''        let takes_request = method_def.param_name.is_some();
        let module_program = module_program.clone();
        let native_plan = native_plan.clone();
        let verb = method_def.verb.clone();
'''
new_plan = '''        let handler_plan = RouteHandlerPlan {
            inline_file,
            module_program: module_program.clone(),
            native_plan: native_plan.clone(),
            takes_request: method_def.param_name.is_some(),
        };
        let verb = method_def.verb.clone();
'''
if text.count(old_plan) != 1:
    raise SystemExit(f"CAP-006 handler plan anchor changed: {text.count(old_plan)}")
text = text.replace(old_plan, new_plan, 1)

old_handler = '''            let inline_file = inline_file.clone();
            let module_program = module_program.clone();
            async move {
                execute(
                    inline_file,
                    module_program,
                    native_plan,
                    takes_request,
                    state,
                    params,
                    query,
                    request,
                )
                .await
            }
'''
new_handler = '''            let handler_plan = handler_plan.clone();
            async move { execute(handler_plan, state, params, query, request).await }
'''
if text.count(old_handler) != 1:
    raise SystemExit(f"CAP-006 handler closure anchor changed: {text.count(old_handler)}")
text = text.replace(old_handler, new_handler, 1)

path.write_text(text, encoding="utf-8")
