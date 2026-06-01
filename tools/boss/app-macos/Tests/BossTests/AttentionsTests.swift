import XCTest
@testable import Boss

/// Pure-model tests for the Attentions feature (attentions.md — App UI). Covers
/// the Swift mirrors of `boss_protocol::AttentionGroup` / `Attention`: wire
/// (snake_case) decoding, optional-field handling, choice parsing, open/state
/// helpers, and produced-artifact-ref decoding for both the single-task
/// (revision / design) and the batch (followup `tasks`) shapes.
///
/// No host app required.
final class AttentionsTests: XCTestCase {

    private func decodeGroup(_ json: String) throws -> AttentionGroup {
        try JSONDecoder().decode(AttentionGroup.self, from: Data(json.utf8))
    }

    private func decodeAttention(_ json: String) throws -> Attention {
        try JSONDecoder().decode(Attention.self, from: Data(json.utf8))
    }

    // MARK: - AttentionGroup decoding

    func testDecodesMinimalQuestionGroupFromWire() throws {
        let json = """
        {
          "id": "atg_abc",
          "product_id": "prod_1",
          "kind": "question",
          "association_project_id": "proj_9",
          "source_kind": "design_doc",
          "source_doc_path": "tools/boss/docs/designs/foo.md",
          "grouping_key": "question|proj_9|doc:tools/boss/docs/designs/foo.md",
          "generation": 0,
          "state": "open",
          "created_at": "2026-05-31T00:00:00Z"
        }
        """
        let group = try decodeGroup(json)
        XCTAssertEqual(group.id, "atg_abc")
        XCTAssertEqual(group.productID, "prod_1")
        XCTAssertEqual(group.kind, "question")
        XCTAssertEqual(group.associationProjectID, "proj_9")
        XCTAssertNil(group.associationTaskID)
        XCTAssertEqual(group.sourceKind, "design_doc")
        XCTAssertEqual(group.sourceDocPath, "tools/boss/docs/designs/foo.md")
        XCTAssertEqual(group.generation, 0)
        XCTAssertEqual(group.state, "open")
        XCTAssertNil(group.shortID)
        XCTAssertNil(group.producedArtifactKind)
    }

    func testGroupOpenStateHelpers() throws {
        func group(state: String) throws -> AttentionGroup {
            try decodeGroup("""
            {"id":"atg_1","product_id":"p","kind":"question","association_task_id":"t",
             "source_kind":"task_transcript","grouping_key":"k","generation":0,
             "state":"\(state)","created_at":"2026-05-31T00:00:00Z"}
            """)
        }
        XCTAssertTrue(try group(state: "open").isOpen)
        XCTAssertTrue(try group(state: "partially_answered").isOpen)
        XCTAssertFalse(try group(state: "actioned").isOpen)
        XCTAssertTrue(try group(state: "actioned").isActioned)
        XCTAssertFalse(try group(state: "dismissed").isOpen)
    }

    func testGroupKindLabel() throws {
        func label(_ kind: String) throws -> String {
            try decodeGroup("""
            {"id":"atg_1","product_id":"p","kind":"\(kind)","association_task_id":"t",
             "source_kind":"task_transcript","grouping_key":"k","generation":0,
             "state":"open","created_at":"2026-05-31T00:00:00Z"}
            """).kindLabel
        }
        XCTAssertEqual(try label("question"), "Question")
        XCTAssertEqual(try label("followup"), "Followup")
        XCTAssertEqual(try label("some_future_kind"), "Some Future Kind")
    }

    // MARK: - produced_artifact_ref decoding

    func testProducedArtifactsSingleTaskShape() throws {
        let group = try decodeGroup("""
        {"id":"atg_1","product_id":"p","kind":"question","association_project_id":"proj",
         "source_kind":"design_doc","grouping_key":"k","generation":0,"state":"actioned",
         "produced_artifact_kind":"revision",
         "produced_artifact_ref":"{\\"task_id\\":\\"tsk_rev\\",\\"short_id\\":42}",
         "created_at":"2026-05-31T00:00:00Z"}
        """)
        let artifacts = group.producedArtifacts
        XCTAssertEqual(artifacts.count, 1)
        XCTAssertEqual(artifacts.first?.taskID, "tsk_rev")
        XCTAssertEqual(artifacts.first?.shortID, 42)
        XCTAssertNil(artifacts.first?.kind)
    }

    func testProducedArtifactsBatchTasksShape() throws {
        let group = try decodeGroup("""
        {"id":"atg_2","product_id":"p","kind":"followup","association_task_id":"t",
         "source_kind":"task_transcript","grouping_key":"k","generation":0,"state":"actioned",
         "produced_artifact_kind":"tasks",
         "produced_artifact_ref":"{\\"tasks\\":[{\\"task_id\\":\\"tsk_a\\",\\"short_id\\":1,\\"kind\\":\\"task\\"},{\\"task_id\\":\\"chr_b\\",\\"short_id\\":2,\\"kind\\":\\"chore\\"}]}",
         "created_at":"2026-05-31T00:00:00Z"}
        """)
        let artifacts = group.producedArtifacts
        XCTAssertEqual(artifacts.count, 2)
        XCTAssertEqual(artifacts[0].taskID, "tsk_a")
        XCTAssertEqual(artifacts[0].kind, "task")
        XCTAssertEqual(artifacts[1].taskID, "chr_b")
        XCTAssertEqual(artifacts[1].kind, "chore")
        XCTAssertEqual(artifacts[1].shortID, 2)
    }

    func testProducedArtifactsEmptyWhenAbsent() throws {
        let group = try decodeGroup("""
        {"id":"atg_3","product_id":"p","kind":"question","association_task_id":"t",
         "source_kind":"manual","grouping_key":"k","generation":0,"state":"open",
         "created_at":"2026-05-31T00:00:00Z"}
        """)
        XCTAssertTrue(group.producedArtifacts.isEmpty)
    }

    // MARK: - Attention decoding + choices

    func testDecodesYesNoQuestionMember() throws {
        let member = try decodeAttention("""
        {"id":"atn_1","group_id":"atg_1","ordinal":1,"answer_state":"open",
         "created_at":"2026-05-31T00:00:00Z","question_type":"yes_no",
         "prompt_text":"Gate extraction behind a flag?","confidence_source":"structured"}
        """)
        XCTAssertEqual(member.questionType, "yes_no")
        XCTAssertEqual(member.promptText, "Gate extraction behind a flag?")
        XCTAssertEqual(member.confidenceSource, "structured")
        XCTAssertFalse(member.isResolved)
        XCTAssertTrue(member.choices.isEmpty)
    }

    func testDecodesMultipleChoiceMemberChoices() throws {
        let member = try decodeAttention("""
        {"id":"atn_2","group_id":"atg_1","ordinal":2,"answer_state":"answered",
         "created_at":"2026-05-31T00:00:00Z","question_type":"multiple_choice",
         "prompt_text":"One table or two?",
         "choice_options":"[\\"one table\\",\\"two tables\\"]",
         "answer":"two tables","confidence_source":"structured"}
        """)
        XCTAssertEqual(member.choices, ["one table", "two tables"])
        XCTAssertEqual(member.answer, "two tables")
        XCTAssertTrue(member.isAnswered)
        XCTAssertTrue(member.isResolved)
    }

    func testChoicesEmptyOnInvalidJSON() throws {
        let member = try decodeAttention("""
        {"id":"atn_3","group_id":"atg_1","ordinal":3,"answer_state":"open",
         "created_at":"2026-05-31T00:00:00Z","question_type":"multiple_choice",
         "choice_options":"not-json","confidence_source":"structured"}
        """)
        XCTAssertTrue(member.choices.isEmpty)
    }

    func testDecodesFollowupMemberAndExtractedFlag() throws {
        let member = try decodeAttention("""
        {"id":"atn_4","group_id":"atg_5","ordinal":1,"answer_state":"skipped",
         "created_at":"2026-05-31T00:00:00Z","proposed_name":"Add retry budget",
         "proposed_description":"Bound CI retries","proposed_effort":"small",
         "proposed_work_kind":"chore","rationale":"noticed during impl",
         "confidence_source":"extracted"}
        """)
        XCTAssertEqual(member.proposedName, "Add retry budget")
        XCTAssertEqual(member.proposedWorkKind, "chore")
        XCTAssertEqual(member.confidenceSource, "extracted")
        XCTAssertTrue(member.isSkipped)
        XCTAssertNil(member.questionType)
    }

    // MARK: - state-glyph-adjacent answer-state helpers

    func testAnswerStateHelpers() throws {
        func member(_ state: String) throws -> Attention {
            try decodeAttention("""
            {"id":"atn_x","group_id":"g","ordinal":1,"answer_state":"\(state)",
             "created_at":"2026-05-31T00:00:00Z","confidence_source":"structured"}
            """)
        }
        XCTAssertTrue(try member("answered").isAnswered)
        XCTAssertTrue(try member("skipped").isSkipped)
        XCTAssertTrue(try member("dismissed").isDismissed)
        XCTAssertFalse(try member("open").isResolved)
        XCTAssertTrue(try member("answered").isResolved)
    }
}
