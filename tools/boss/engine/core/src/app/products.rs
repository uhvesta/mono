//! `FrontendRequest` handlers — product CRUD and product-level settings.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_create_product(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateProduct { input } = req else {
        unreachable!()
    };
    match work_db.create_product(input) {
        Ok(product) => {
            let item = WorkItem::Product(product);
            let revision = publish_work_invalidation(
                &server_state,
                &session_id,
                &request_id,
                vec![TOPIC_WORK_PRODUCTS.to_owned(), work_product_topic(&work_item_id(&item))],
                "product_created",
                Some(work_item_product_id(&item)),
                vec![work_item_id(&item)],
            )
            .await;
            send_response_with_revision(&sink, &request_id, revision, FrontendEvent::WorkItemCreated { item });
        }
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            );
        }
    }
}

pub(super) async fn handle_list_products(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListProducts = req else {
        unreachable!()
    };
    match work_db.list_products() {
        Ok(products) => {
            send_response_with_revision(
                &sink,
                &request_id,
                server_state.current_work_revision(),
                FrontendEvent::ProductsList { products },
            );
        }
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            );
        }
    }
}

pub(super) async fn handle_set_product_default_model(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetProductDefaultModel { product_id, model } = req else {
        unreachable!()
    };
    {
        match work_db.set_product_default_model(&product_id, model.as_deref()) {
            Ok(product) => {
                let item = WorkItem::Product(product);
                let pid = work_item_product_id(&item);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&pid)],
                    "product_default_model_set",
                    Some(pid),
                    vec![work_item_id(&item)],
                )
                .await;
                send_response_with_revision(&sink, &request_id, revision, FrontendEvent::WorkItemUpdated { item });
            }
            Err(err) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            ),
        }
    }
}

pub(super) async fn handle_set_product_default_driver(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetProductDefaultDriver { product_id, driver } = req else {
        unreachable!()
    };
    {
        match work_db.set_product_default_driver(&product_id, driver.as_deref()) {
            Ok(product) => {
                let item = WorkItem::Product(product);
                let pid = work_item_product_id(&item);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&pid)],
                    "product_default_driver_set",
                    Some(pid),
                    vec![work_item_id(&item)],
                )
                .await;
                send_response_with_revision(&sink, &request_id, revision, FrontendEvent::WorkItemUpdated { item });
            }
            Err(err) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            ),
        }
    }
}

pub(super) async fn handle_set_product_editorial_rules(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetProductEditorialRules { input } = req else {
        unreachable!()
    };
    {
        match work_db.set_product_editorial_rules(&input.product_id, input.rules.as_ref()) {
            Ok(product) => {
                let item = WorkItem::Product(product);
                let pid = work_item_product_id(&item);
                let revision = publish_work_invalidation(
                    &server_state,
                    &session_id,
                    &request_id,
                    vec![work_product_topic(&pid)],
                    "product_editorial_rules_set",
                    Some(pid),
                    vec![work_item_id(&item)],
                )
                .await;
                send_response_with_revision(&sink, &request_id, revision, FrontendEvent::WorkItemUpdated { item });
            }
            Err(err) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            ),
        }
    }
}

pub(super) async fn handle_evaluate_editorial_rules(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::EvaluateEditorialRules {
        product_id,
        body,
        title,
    } = req
    else {
        unreachable!()
    };

    let product = match work_db.get_product(&product_id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("product {product_id} not found"),
                },
            );
            return;
        }
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            );
            return;
        }
    };

    let rules = product.editorial_rules.clone().unwrap_or_default();
    let compiled = match boss_editorial::CompiledRules::compile(rules) {
        Ok(c) => c,
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("invalid redaction regex in editorial_rules: {err}"),
                },
            );
            return;
        }
    };

    let decision = boss_editorial::evaluate(&body, title.as_deref().unwrap_or(""), &compiled, None);
    let (decision_str, findings, rewritten_body) = match decision {
        boss_editorial::EditorialDecision::Allow => ("allow", vec![], None),
        boss_editorial::EditorialDecision::Rewrite {
            body: new_body,
            findings,
            ..
        } => (
            "rewrite",
            findings.iter().map(|f| f.description.clone()).collect(),
            Some(new_body),
        ),
        boss_editorial::EditorialDecision::Block { findings } => {
            ("deny", findings.iter().map(|f| f.description.clone()).collect(), None)
        }
    };

    send_response(
        &sink,
        &request_id,
        FrontendEvent::EditorialRulesEvaluated {
            product_id,
            decision: decision_str.to_owned(),
            findings,
            rewritten_body,
        },
    );
}
