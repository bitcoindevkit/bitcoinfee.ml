use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::TryInto;
use std::time::Duration;

use log::*;

use serde_derive::{Deserialize, Serialize};

use yew::events::ChangeData;
use yew::format::{Json, Nothing};
use yew::prelude::*;
use yew::services::fetch::{FetchService, FetchTask, Request, Response};
use yew::services::timeout::{TimeoutService, TimeoutTask};

use js_sys::Date;

use bitcoin_fee_model::bitcoin;
use bitcoin_fee_model::{get_model_high, get_model_low, process_blocks, FeeModel, Size64};

use bitcoin::consensus::encode::deserialize;
use bitcoin::hash_types::BlockHash;
use bitcoin::Block;

const MAX_CONCURRENT_REQ: usize = 4;
// const KEY: &str = "bitcoinfee.ml.blocks";

pub struct App {
    link: ComponentLink<Self>,
    // storage: StorageService,
    state: State,

    task_id: usize,
    fetch_tasks: HashMap<usize, FetchTask>,
    queued_tasks: VecDeque<EsploraRequest>,
    process_blocks: Option<TimeoutTask>,
}

#[derive(Debug)]
pub enum EsploraRequest {
    GetLatestBlocks,
    GetBlock(BlockHash),
}

impl EsploraRequest {
    fn get_url(&self) -> String {
        match self {
            EsploraRequest::GetLatestBlocks => "https://mempool.space/api/blocks".to_string(),
            EsploraRequest::GetBlock(hash) => {
                format!("https://mempool.space/api/block/{}/raw", hash)
            }
        }
    }

    fn get_fetch_task(&self, link: &ComponentLink<App>, id: usize) -> FetchTask {
        let req = Request::get(&self.get_url())
            .body(Nothing)
            .expect("Could not build request.");

        match self {
            EsploraRequest::GetLatestBlocks => FetchService::fetch(
                req,
                link.callback(move |response: Response<Json<Result<_, anyhow::Error>>>| {
                    let Json(data) = response.into_body();
                    Msg::ReceivedLatestBlocks(id, data)
                }),
            ),
            EsploraRequest::GetBlock(_) => FetchService::fetch_binary(
                req,
                link.callback(move |response: Response<Result<_, anyhow::Error>>| {
                    let data = response.into_body();
                    Msg::ReceivedBlock(id, data)
                }),
            ),
        }
        .expect("Could not start request.")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EsploraBlock {
    id: BlockHash,
    height: usize,
}

pub struct State {
    model: FeeModel<Size64>,
    blocks: Option<HashMap<BlockHash, Block>>,
    blocks_index: Vec<EsploraBlock>,
    fee_rates: Option<(Vec<f64>, u32)>,
    target: Option<u16>,
    estimate: Option<f32>,
    blocks_left: usize,
}

#[derive(Debug)]
pub enum Msg {
    SetTarget(ChangeData),
    Estimate,

    GetLatestBlocks,
    ReceivedLatestBlocks(usize, Result<Vec<EsploraBlock>, anyhow::Error>),
    ReceivedBlock(usize, Result<Vec<u8>, anyhow::Error>),
    ProcessBlocks,
}

impl Component for App {
    type Message = Msg;
    type Properties = ();

    fn create(_: Self::Properties, link: ComponentLink<Self>) -> Self {
        // See below, storage is disabled for the time being
        //
        // let storage = StorageService::new(Area::Local).unwrap();
        // let blocks = {
        //     if let Json(Ok(blocks)) = storage.restore::<Json<Result<Vec<String>, _>>>(KEY) {
        //         Some(
        //             blocks
        //                 .into_iter()
        //                 .map(|b| -> Result<Block, Box<dyn std::error::Error>> {
        //                     Ok(deserialize(&Vec::<u8>::from_hex(&b)?)?)
        //                 })
        //                 .filter_map(|r| match r {
        //                     Ok(b) => Some((b.block_hash(), b)),
        //                     Err(e) => {
        //                         warn!("Error loading blocks from local storage: {:?}", e);
        //                         None
        //                     }
        //                 })
        //                 .collect::<HashMap<_, _>>(),
        //         )
        //     } else {
        //         None
        //     }
        // };
        let state = State {
            model: FeeModel::new(get_model_low(), get_model_high()),
            blocks: None,
            blocks_index: vec![],
            fee_rates: None,
            target: None,
            estimate: None,
            blocks_left: 0,
        };

        link.send_message(Msg::GetLatestBlocks);

        App {
            link,
            // storage,
            state,

            task_id: 0,
            fetch_tasks: HashMap::new(),
            queued_tasks: VecDeque::new(),
            process_blocks: None,
        }
    }

    fn change(&mut self, _props: Self::Properties) -> ShouldRender {
        false
    }

    fn update(&mut self, msg: Self::Message) -> ShouldRender {
        match msg {
            Msg::GetLatestBlocks => {
                self.clear_all_req();
                self.enqueue_request(EsploraRequest::GetLatestBlocks);
            }
            Msg::ReceivedLatestBlocks(id, Ok(latest_blocks)) => {
                self.clear_request(id);

                self.state.blocks_left = 0;

                let block_hashes = latest_blocks.iter().map(|b| b.id).collect::<HashSet<_>>();
                let blocks = self.state.blocks.get_or_insert(Default::default());
                blocks.retain(|k, _| block_hashes.contains(k));

                info!("Retained {} blocks", blocks.len());

                for block in &latest_blocks {
                    if !self.state.blocks.as_ref().unwrap().contains_key(&block.id) {
                        self.state.blocks_left += 1;

                        self.enqueue_request(EsploraRequest::GetBlock(block.id));
                    }
                }

                self.state.blocks_index = latest_blocks;
            }
            Msg::ReceivedBlock(id, Ok(block)) => {
                self.clear_request(id);

                let block: Block =
                    deserialize(&block).expect("Invalid block received from the APIs");

                let blocks = self.state.blocks.get_or_insert(Default::default());
                blocks.insert(block.block_hash(), block);

                self.state.blocks_left -= 1;

                if self.state.blocks_left == 0 {
                    // Process blocks at the next tick to update the UI first
                    //
                    // Ideally this should run on a background worker, but they are not 100%
                    // working with yew yet.
                    self.process_blocks = Some(TimeoutService::spawn(
                        Duration::from_millis(0),
                        self.link.callback(|_| Msg::ProcessBlocks),
                    ));
                }
            }
            Msg::ProcessBlocks => {
                self.process_blocks = None;

                let blocks = self.state.blocks.get_or_insert(Default::default());

                // There's not enough space in the local storage for 10 blocks. We should use
                // IndexedDB, but even gloo-storage doesn't expose APIs for it, and for the time
                // being I don't want to deal with low level APIs directly.
                //
                // info!("Saving to local storage...");
                // let blocks_vec = blocks.values().map(|b| serialize(&b).to_hex()).collect::<Vec<_>>();
                // self.storage.store(KEY, Json(&blocks_vec));
                // info!("Done");

                // Compute fee_rates
                self.state.fee_rates = Some(
                    process_blocks(
                        &self
                            .state
                            .blocks_index
                            .iter()
                            .rev()
                            .filter_map(|b| blocks.get(&b.id))
                            .cloned()
                            .collect::<Vec<_>>()
                            .try_into()
                            .expect("Wrong number of blocks found in the index"),
                    )
                    .expect("Failed to process blocks"),
                );
            }
            Msg::SetTarget(ChangeData::Value(s)) => {
                self.state.target = s.parse().ok();
            }
            Msg::Estimate if self.state.target.is_some() && self.state.fee_rates.is_some() => {
                let target = self.state.target.unwrap();
                let fee_rates = self.state.fee_rates.as_ref().unwrap();

                info!("Running estimation for target = {}", target);

                let ts = (Date::now() / 1e3) as u32;
                self.state.estimate = Some(
                    self.state
                        .model
                        .estimate(target, Some(ts), &fee_rates.0, fee_rates.1)
                        .expect("Unable to run the fee estimation"),
                );
            }
            e => {
                error!("{:?}", e);
            }
        }

        true
    }

    fn view(&self) -> Html {
        html! {
            <>
                <h2>{ "Run" }</h2>
                <input type="text" onchange=self.link.callback(Msg::SetTarget) /><button onclick=self.link.callback(|_| Msg::Estimate) disabled=self.state.target.is_none()>{ "Estimate" }</button> <br/>
                <button onclick=self.link.callback(|_| Msg::GetLatestBlocks)>{ "Refresh blocks" }</button>
                <h2>{ "State" }</h2>
                <pre>{ format!("blocks: {:?}", self.state.blocks.as_ref().map(|map| map.len())) }</pre>
                <pre>{ format!("blocks_index: {:#?}", self.state.blocks_index) }</pre>
                <pre>{ format!("fee_rates: {:?}", self.state.fee_rates.as_ref().map(|(f, ts)| (f.len(), ts))) }</pre>
                <pre>{ format!("target: {:?}", self.state.target) }</pre>
                <pre>{ format!("estimate: {:?}", self.state.estimate) }</pre>
                <pre>{ format!("blocks_left: {}", self.state.blocks_left) }</pre>
                <h2>{ "Tasks" }</h2>
                <pre>{ format!("fetch_tasks: {:?}", self.fetch_tasks) }</pre>
                <pre>{ format!("queued_tasks: {:?}", self.queued_tasks) }</pre>
                <pre>{ format!("process_blocks: {:?}", self.process_blocks) }</pre>
            </>
        }
    }
}

impl App {
    fn enqueue_request(&mut self, request: EsploraRequest) {
        if self.fetch_tasks.len() < MAX_CONCURRENT_REQ {
            self.start_request(request);
        } else {
            self.queued_tasks.push_back(request);
        }
    }

    fn start_request(&mut self, request: EsploraRequest) {
        let id = self.task_id;
        self.task_id += 1;

        self.fetch_tasks
            .insert(id, request.get_fetch_task(&self.link, id));
    }

    fn clear_request(&mut self, id: usize) {
        self.fetch_tasks.remove(&id);
        if let Some(task) = self.queued_tasks.pop_front() {
            self.start_request(task);
        }
    }

    fn clear_all_req(&mut self) {
        self.fetch_tasks.clear();
        self.queued_tasks.clear();
    }
}
