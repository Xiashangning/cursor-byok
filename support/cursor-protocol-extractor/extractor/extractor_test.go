// extractor_test.go 验证压缩 bundle 的别名解析与现代工厂语法提取行为。
package main

import "testing"

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
