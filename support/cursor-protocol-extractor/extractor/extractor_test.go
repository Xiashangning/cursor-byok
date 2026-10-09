// extractor_test.go 验证压缩 bundle 的别名解析与现代工厂语法提取行为。
package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestGeneratedProtoEndsWithOneNewline(t *testing.T) {
	directory := t.TempDir()
	generateProtoFile("example.v1", nil, nil, nil, nil, directory)
	data, err := os.ReadFile(filepath.Join(directory, "example_v1.proto"))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasSuffix(string(data), "\n") || strings.HasSuffix(string(data), "\n\n") {
		t.Fatalf("unexpected trailing whitespace: %q", data)
	}
}

// TestWebpackExportAliasResolvesServiceMessageType 验证 Webpack 导出别名可解析服务消息。
func TestWebpackExportAliasResolvesServiceMessageType(t *testing.T) {
	const bundle = `
1:(e,t,n)=>{
  n.d(t,{KS:()=>T,_B:()=>r});
  var r;
  class T {}
  T.typeName="agent.v1.AgentClientMessage";
  n.proto3.util.setEnumType(r,"agent.v1.DiagnosticSeverity",[]);
},
2:(e,t,n)=>{
  var r=n(1);
  const service={typeName:"agent.v1.AgentService",methods:{run:{name:"Run",I:r.KS,O:r.KS,kind:n.MethodKind.BiDiStreaming}}};
}`

	moduleStarts := buildModuleStarts(bundle)
	messages := []Message{{
		TypeName:     "agent.v1.AgentClientMessage",
		VarName:      "T",
		InternalName: "T",
		Package:      "agent.v1",
		Pos:          35,
		ModuleStart:  moduleStartForPos(moduleStarts, 35),
	}}
	enums := []Enum{{
		TypeName:    "agent.v1.DiagnosticSeverity",
		VarName:     "r",
		Package:     "agent.v1",
		Pos:         100,
		ModuleStart: moduleStartForPos(moduleStarts, 100),
	}}

	resolver := newTypeResolver(messages, enums, buildAliasIndex(bundle, moduleStarts), buildWebpackExportAliasIndex(bundle, moduleStarts))
	resolver.moduleImports = buildModuleImportIndex(bundle, moduleStarts)
	typeName, ok := resolver.ResolveTypeName("r.KS", len(bundle)-1, moduleStartForPos(moduleStarts, len(bundle)-1), "agent.v1", "message")
	if !ok {
		t.Fatal("expected webpack export alias to resolve")
	}
	if typeName != "agent.v1.AgentClientMessage" {
		t.Fatalf("resolved r.KS to %q, want agent.v1.AgentClientMessage", typeName)
	}
}

// TestModernFactorySyntaxExtractsInAppAdServiceTypes 验证现代工厂语法提取完整服务类型。
func TestModernFactorySyntaxExtractsInAppAdServiceTypes(t *testing.T) {
	const bundle = `
42:(e,t,n)=>{
  var HasSeenAdRequest=n.makeMessageType("aiserver.v1.HasSeenAdRequest",()=>[{no:1,name:"ad_id",kind:"scalar",T:9}]),
      HasSeenAdResponse=n.makeMessageType("aiserver.v1.HasSeenAdResponse",()=>[{no:1,name:"has_seen",kind:"scalar",T:8}]),
      MarkAdAsSeenResponse=n.makeMessageType("aiserver.v1.MarkAdAsSeenResponse",[]),
      Placement=n.makeEnum("aiserver.v1.InAppAdPlacement",[{no:0,name:"IN_APP_AD_PLACEMENT_UNSPECIFIED",localName:"UNSPECIFIED"}]),
      InAppAdService={typeName:"aiserver.v1.InAppAdService",methods:{hasSeenAd:{name:"HasSeenAd",I:HasSeenAdRequest,O:HasSeenAdResponse,kind:n.MethodKind.Unary},markAdAsSeen:{name:"MarkAdAsSeen",I:HasSeenAdRequest,O:MarkAdAsSeenResponse,kind:n.MethodKind.Unary}}};
}`

	moduleStarts := buildModuleStarts(bundle)
	messages := extractMessages(bundle, moduleStarts)
	enums := extractEnums(bundle, moduleStarts)
	services := extractServices(bundle, moduleStarts)

	if len(messages) != 3 {
		t.Fatalf("extracted %d messages, want 3", len(messages))
	}
	if len(messages[0].Fields) != 1 || messages[0].Fields[0].Name != "ad_id" {
		t.Fatalf("unexpected request fields: %#v", messages[0].Fields)
	}
	if len(enums) != 1 || enums[0].TypeName != "aiserver.v1.InAppAdPlacement" {
		t.Fatalf("unexpected enums: %#v", enums)
	}
	if len(services) != 1 || len(services[0].Methods) != 2 {
		t.Fatalf("unexpected services: %#v", services)
	}

	resolver := newTypeResolver(messages, enums, buildAliasIndex(bundle, moduleStarts), buildWebpackExportAliasIndex(bundle, moduleStarts))
	method := services[0].Methods[0]
	input, inputOK := resolver.ResolveTypeName(method.InputType, services[0].Pos, services[0].ModuleStart, services[0].Package, "message")
	output, outputOK := resolver.ResolveTypeName(method.OutputType, services[0].Pos, services[0].ModuleStart, services[0].Package, "message")
	if !inputOK || input != "aiserver.v1.HasSeenAdRequest" {
		t.Fatalf("resolved input to %q (ok=%v)", input, inputOK)
	}
	if !outputOK || output != "aiserver.v1.HasSeenAdResponse" {
		t.Fatalf("resolved output to %q (ok=%v)", output, outputOK)
	}
}

// TestPureAnnotatedFactoryDeclarations 验证 /*@__PURE__*/ 注释不再阻断工厂声明提取。
// 未压缩构建(如 cursor-resolver 浏览器 bundle)把注释放在 = 与模块路径之间。
func TestPureAnnotatedFactoryDeclarations(t *testing.T) {
	const bundle = `
1:(e,t,n)=>{
  const Req = /*@__PURE__*/ n.proto3.makeMessageType(
    "aiserver.v1.PureReq",
    () => [
      { no: 1, name: "ad_id", kind: "scalar", T: 9 },
    ],
  );
  const Res = /*@__PURE__*/ n.proto3.makeMessageType("aiserver.v1.PureRes", []);
  const Role = /*@__PURE__*/ n.proto3.makeEnum(
    "aiserver.v1.PureRole",
    [
      {no: 0, name: "PURE_ROLE_UNSPECIFIED", localName: "UNSPECIFIED"},
    ],
  );
}`

	moduleStarts := buildModuleStarts(bundle)
	messages := extractMessages(bundle, moduleStarts)
	enums := extractEnums(bundle, moduleStarts)

	if len(messages) != 2 {
		t.Fatalf("extracted %d messages, want 2", len(messages))
	}
	if messages[0].TypeName != "aiserver.v1.PureReq" || len(messages[0].Fields) != 1 {
		t.Fatalf("unexpected PURE message extraction: %#v", messages[0])
	}
	if messages[1].TypeName != "aiserver.v1.PureRes" || len(messages[1].Fields) != 0 {
		t.Fatalf("unexpected PURE empty message extraction: %#v", messages[1])
	}
	if len(enums) != 1 || enums[0].TypeName != "aiserver.v1.PureRole" || len(enums[0].Values) != 1 {
		t.Fatalf("unexpected PURE enum extraction: %#v", enums)
	}
}

// TestModuleScopeWinsOverPackagePreference 验证模块内同名符号优先于同包其它模块的符号。
// 复现 QueuedFollowup.blob_data:两个不同模块各声明一个 Mn,字段必须绑定本模块的定义。
func TestModuleScopeWinsOverPackagePreference(t *testing.T) {
	const bundle = `1:(e,t,n)=>{
  Mn.runtime=n.proto3,Mn.typeName="internapi.v1.BlobData",Mn.fields=n.proto3.util.newFieldList(()=>[{no:1,name:"blob_id",kind:"scalar",T:12}]);
  Pd.runtime=n.proto3,Pd.typeName="aiserver.v1.QueuedFollowup",Pd.fields=n.proto3.util.newFieldList(()=>[{no:9,name:"blob_data",kind:"message",T:Mn,repeated:!0}]);
},
2:(e,t,n)=>{
  Mn.runtime=n.proto3,Mn.typeName="aiserver.v1.TaskSubagentReturnValue",Mn.fields=n.proto3.util.newFieldList(()=>[{no:1,name:"summary",kind:"scalar",T:9}]);
}`

	moduleStarts := buildModuleStarts(bundle)
	messages := extractMessages(bundle, moduleStarts)
	if len(messages) != 3 {
		t.Fatalf("extracted %d messages, want 3: %#v", len(messages), messages)
	}

	var queued Message
	for _, msg := range messages {
		if msg.TypeName == "aiserver.v1.QueuedFollowup" {
			queued = msg
		}
	}
	if queued.TypeName == "" {
		t.Fatal("QueuedFollowup not extracted")
	}

	resolver := newTypeResolver(messages, nil, buildAliasIndex(bundle, moduleStarts), buildWebpackExportAliasIndex(bundle, moduleStarts))
	resolver.moduleImports = buildModuleImportIndex(bundle, moduleStarts)
	field := queued.Fields[0]
	if field.Name != "blob_data" {
		t.Fatalf("unexpected first field: %#v", field)
	}
	resolved, ok := resolver.ResolveTypeName(field.T.(string), queued.Pos, queued.ModuleStart, queued.Package, "message")
	if !ok {
		t.Fatal("blob_data type did not resolve")
	}
	if resolved != "internapi.v1.BlobData" {
		t.Fatalf("blob_data resolved to %q, want internapi.v1.BlobData (module-local declaration)", resolved)
	}
}
