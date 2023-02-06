use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::str::FromStr;

use crate::input::VMInputT;
use crate::rand;
use crate::state_input::StagedVMState;
use bytes::Bytes;
use libafl::prelude::ObserversTuple;
use primitive_types::{H160, H256, U256};
use revm::db::BenchmarkDB;
use revm::Return::{Continue, Revert};
use revm::{
    Bytecode, CallInputs, Contract, CreateInputs, Env, Gas, Host, Interpreter, LatestSpec, Return,
    SelfDestructResult, Spec,
};
use serde::{Deserialize, Serialize};

pub const MAP_SIZE: usize = 1024;

pub static mut jmp_map: [u8; MAP_SIZE] = [0; MAP_SIZE];
pub static mut read_map: [bool; MAP_SIZE] = [false; MAP_SIZE];
pub static mut write_map: [u8; MAP_SIZE] = [0; MAP_SIZE];

pub const RW_SKIPPER_PERCT_IDX: usize = 100;
pub const RW_SKIPPER_AMT: usize = MAP_SIZE - RW_SKIPPER_PERCT_IDX;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VMState {
    state: HashMap<H160, HashMap<U256, U256>>,
    // If control leak happens, we add state with incomplete execution to the corpus
    // More than one when the control is leaked again with the call based on the incomplete state
    pub post_execution: Vec<(Vec<U256>, usize)>,
}

impl VMState {
    pub(crate) fn new() -> Self {
        Self {
            state: HashMap::new(),
            post_execution: vec![],
        }
    }

    fn get(&self, address: &H160) -> Option<&HashMap<U256, U256>> {
        self.state.get(address)
    }

    fn get_mut(&mut self, address: &H160) -> Option<&mut HashMap<U256, U256>> {
        self.state.get_mut(address)
    }

    fn insert(&mut self, address: H160, storage: HashMap<U256, U256>) {
        self.state.insert(address, storage);
    }
}

use crate::state::{FuzzState, HasHashToAddress};
pub use jmp_map as JMP_MAP;
pub use read_map as READ_MAP;
pub use write_map as WRITE_MAP;

#[derive(Clone, Debug)]
pub struct FuzzHost {
    pub data: VMState,
    // these are internal to the host
    env: Env,
    code: HashMap<H160, Bytecode>,
    hash_to_address: HashMap<[u8; 4], H160>,
    _pc: usize,
    pc_to_addresses: HashMap<usize, HashSet<H160>>,
}

// hack: I don't want to change evm internal to add a new type of return
// this return type is never used as we disabled gas
const ControlLeak: Return = Return::FatalExternalError;
const ACTIVE_MATCH_EXT_CALL: bool = true;
const CONTROL_LEAK_DETECTION: bool = true;

// if a PC transfers control to >10 addresses, we consider call at this PC to be unbounded
const CONTROL_LEAK_THRESHOLD: usize = 10;

impl FuzzHost {
    pub fn new() -> Self {
        Self {
            data: VMState::new(),
            env: Env::default(),
            code: HashMap::new(),
            hash_to_address: HashMap::new(),
            _pc: 0,
            pc_to_addresses: HashMap::new(),
        }
    }

    pub fn initalize<S>(&mut self, state: &S)
    where
        S: HasHashToAddress,
    {
        self.hash_to_address = state.get_hash_to_address().clone();
    }

    pub fn set_code(&mut self, address: H160, code: Bytecode) {
        self.code.insert(address, code.to_analysed::<LatestSpec>());
    }
}

macro_rules! process_rw_key {
    ($key:ident) => {
        if $key > U256::from(RW_SKIPPER_PERCT_IDX) {
            $key >>= 4;
            $key %= U256::from(RW_SKIPPER_AMT);
            $key += U256::from(RW_SKIPPER_PERCT_IDX);
            $key.as_usize() % MAP_SIZE
        } else {
            $key.as_usize() % MAP_SIZE
        }
    };
}

macro_rules! u256_to_u8 {
    ($key:ident) => {
        ($key.as_u64() << 4 % 255) as u8
    };
}
impl Host for FuzzHost {
    const INSPECT: bool = true;
    type DB = BenchmarkDB;
    fn step(&mut self, interp: &mut Interpreter, is_static: bool) -> Return {
        unsafe {
            // println!("{}", *interp.instruction_pointer);
            match *interp.instruction_pointer {
                0x57 => { // JUMPI
                    let jump_dest = if interp.stack.peek(0).expect("stack underflow").is_zero() {
                        interp.stack.peek(1).expect("stack underflow").as_u64()
                    } else {
                        1
                    };
                    let idx = (interp.program_counter() ^ (jump_dest as usize)) % MAP_SIZE;
                    if jmp_map[idx] < 255 {
                        jmp_map[idx] += 1;
                    }
                }

                0x55 => { // SSTORE
                    let mut key = interp.stack.peek(0).expect("stack underflow");
                    let value = interp.stack.peek(1).expect("stack underflow");
                    WRITE_MAP[process_rw_key!(key)] = u256_to_u8!(value);
                }

                0x54 => { // SLOAD
                    let mut key = interp.stack.peek(0).expect("stack underflow");
                    READ_MAP[process_rw_key!(key)] = true;
                }

                0xf1 | 0xf2 | 0xf4 | 0xfa => {
                    self._pc = interp.program_counter();
                }
                _ => {}
            }
        }
        return Continue;
    }

    fn step_end(&mut self, interp: &mut Interpreter, is_static: bool, ret: Return) -> Return {
        return Continue;
    }

    fn env(&mut self) -> &mut Env {
        return &mut self.env;
    }

    fn load_account(&mut self, address: H160) -> Option<(bool, bool)> {
        Some((
            true,
            true, // self.data.contains_key(&address) || self.code.contains_key(&address),
        ))
    }

    fn block_hash(&mut self, number: U256) -> Option<H256> {
        println!("blockhash {}", number);

        Some(
            H256::from_str("0x0000000000000000000000000000000000000000000000000000000000000000")
                .unwrap(),
        )
    }

    fn balance(&mut self, address: H160) -> Option<(U256, bool)> {
        println!("balance");

        Some((U256::max_value(), true))
    }

    fn code(&mut self, address: H160) -> Option<(Bytecode, bool)> {
        println!("code");
        match self.code.get(&address) {
            Some(code) => Some((code.clone(), true)),
            None => Some((Bytecode::new(), true)),
        }
    }

    fn code_hash(&mut self, address: H160) -> Option<(H256, bool)> {
        Some((
            H256::from_str("0x0000000000000000000000000000000000000000000000000000000000000000")
                .unwrap(),
            true,
        ))
    }

    fn sload(&mut self, address: H160, index: U256) -> Option<(U256, bool)> {
        match self.data.get(&address) {
            Some(account) => Some((account.get(&index).unwrap_or(&U256::zero()).clone(), true)),
            None => Some((U256::zero(), true)),
        }
    }

    fn sstore(
        &mut self,
        address: H160,
        index: U256,
        value: U256,
    ) -> Option<(U256, U256, U256, bool)> {
        match self.data.get_mut(&address) {
            Some(account) => {
                account.insert(index, value);
            }
            None => {
                let mut account = HashMap::new();
                account.insert(index, value);
                self.data.insert(address, account);
            }
        };
        Some((U256::from(0), U256::from(0), U256::from(0), true))
    }

    fn log(&mut self, address: H160, topics: Vec<H256>, data: Bytes) {}

    fn selfdestruct(&mut self, address: H160, target: H160) -> Option<SelfDestructResult> {
        return Some(SelfDestructResult::default());
    }

    fn create<SPEC: Spec>(
        &mut self,
        inputs: &mut CreateInputs,
    ) -> (Return, Option<H160>, Gas, Bytes) {
        unsafe {
            println!("create");
        }
        return (
            Continue,
            Some(H160::from_str("0x0000000000000000000000000000000000000000").unwrap()),
            Gas::new(0),
            Bytes::new(),
        );
    }

    fn call<SPEC: Spec>(&mut self, input: &mut CallInputs) -> (Return, Gas, Bytes) {
        if CONTROL_LEAK_DETECTION {
            assert!(self._pc != 0);
            if !self.pc_to_addresses.contains_key(&self._pc) {
                self.pc_to_addresses.insert(self._pc, HashSet::new());
            }
            if self.pc_to_addresses.get(&self._pc).unwrap().len() > CONTROL_LEAK_THRESHOLD {
                return (ControlLeak, Gas::new(0), Bytes::new());
            }
            self.pc_to_addresses
                .get_mut(&self._pc)
                .unwrap()
                .insert(input.contract);
        }

        if ACTIVE_MATCH_EXT_CALL == true {
            let contract_loc = self
                .hash_to_address
                .get(input.input.slice(0..4).to_vec().as_slice())
                .unwrap();
            let mut interp = Interpreter::new::<LatestSpec>(
                Contract::new_with_context::<LatestSpec>(
                    input.input.clone(),
                    self.code.get(contract_loc).unwrap().clone(),
                    &input.context,
                ),
                1e10 as u64,
            );
            let ret = interp.run::<FuzzHost, LatestSpec>(self);
            return (ret, Gas::new(0), interp.return_value());
        }

        // default behavior
        match self.code.get(&input.contract) {
            Some(code) => {
                let mut interp = Interpreter::new::<LatestSpec>(
                    Contract::new_with_context::<LatestSpec>(
                        input.input.clone(),
                        code.clone(),
                        &input.context,
                    ),
                    1e10 as u64,
                );
                let ret = interp.run::<FuzzHost, LatestSpec>(self);
                return (ret, Gas::new(0), interp.return_value());
            }
            None => {
                return (Revert, Gas::new(0), Bytes::new());
            }
        }

        return (Continue, Gas::new(0), Bytes::new());
    }
}

#[derive(Debug, Clone)]
pub struct EVMExecutor<I, S> {
    pub host: FuzzHost,
    deployer: H160,
    phandom: PhantomData<(I, S)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub output: Bytes,
    pub reverted: bool,
    pub new_state: StagedVMState,
}

#[derive(Clone, Debug)]
pub struct IntermediateExecutionResult {
    pub output: Bytes,
    pub new_state: VMState,
    pub pc: usize,
    pub ret: Return,
    pub stack: Vec<U256>,
}

impl ExecutionResult {
    pub fn empty_result() -> Self {
        Self {
            output: Bytes::new(),
            reverted: false,
            new_state: StagedVMState::new_uninitialized(),
        }
    }
}

impl<I, S> EVMExecutor<I, S>
where
    I: VMInputT,
{
    pub fn new(FuzzHost: FuzzHost, deployer: H160) -> Self {
        Self {
            host: FuzzHost,
            deployer,
            phandom: PhantomData,
        }
    }

    pub fn finish_execution(&mut self, result: &ExecutionResult, input: &I) -> ExecutionResult {
        let mut new_state = result.new_state.state.clone();
        let mut last_output = result.output.clone();
        for post_exec in result.new_state.state.post_execution.clone() {
            // there are two cases
            // / 1. the post_exec finishes
            // / 2. the post_exec leads to a new control leak (i.e., there are more than 1
            //      control leak in this function)

            let mut recovering_stack = post_exec.0;
            // we need push the output of CALL instruction
            recovering_stack.push(U256::from(1));
            let r = self.execute_from_pc(
                input.get_contract(),
                input.get_caller(),
                &new_state,
                input.to_bytes(),
                // todo(@shou !important) do we need to increase pc?
                Some((recovering_stack, post_exec.1 + 1)),
            );
            last_output = r.output;
            if r.ret == Return::Return {
                continue;
            }
            if r.ret == ControlLeak {
                panic!("more than one reentrancy in a function! not supported yet");
            }
            if r.ret != Return::Return || r.ret != Return::Stop {
                return ExecutionResult {
                    output: last_output,
                    reverted: true,
                    new_state: StagedVMState {
                        state: new_state,
                        stage: result.new_state.stage,
                        initialized: result.new_state.initialized,
                    },
                };
            }
        }
        return ExecutionResult {
            output: last_output,
            reverted: false,
            new_state: StagedVMState {
                state: new_state,
                stage: result.new_state.stage,
                initialized: result.new_state.initialized,
            },
        };
    }

    pub fn deploy(&mut self, code: Bytecode, constructor_args: Bytes) -> H160 {
        let deployed_address = rand::generate_random_address();
        let deployer = Contract::new::<LatestSpec>(
            constructor_args,
            code,
            deployed_address,
            self.deployer,
            U256::from(0),
        );
        let mut interp = Interpreter::new::<LatestSpec>(deployer, 1e10 as u64);
        let r = interp.run::<FuzzHost, LatestSpec>(&mut self.host);
        assert_eq!(r, Return::Return);
        self.host.set_code(
            deployed_address,
            Bytecode::new_raw(interp.return_value()).to_analysed::<LatestSpec>(),
        );
        deployed_address
    }

    pub fn execute<OT>(
        &mut self,
        contract_address: H160,
        caller: H160,
        state: &VMState,
        data: Bytes,
        observers: &mut OT,
    ) -> ExecutionResult
    where
        OT: ObserversTuple<I, S>,
    {
        let r = self.execute_from_pc(contract_address, caller, state, data, None);
        match r.ret {
            ControlLeak => {
                self.host.data.post_execution.push((r.stack, r.pc));
            }
            _ => {}
        }
        return ExecutionResult {
            output: r.output,
            reverted: r.ret != Return::Return,
            new_state: StagedVMState::new_with_state(r.new_state),
        };
    }

    pub fn execute_from_pc(
        &mut self,
        contract_address: H160,
        caller: H160,
        state: &VMState,
        data: Bytes,
        post_exec: Option<(Vec<U256>, usize)>,
    ) -> IntermediateExecutionResult {
        self.host.data = state.clone();
        let call = Contract::new::<LatestSpec>(
            data,
            self.host
                .code
                .get(&contract_address)
                .expect("no code")
                .clone(),
            contract_address,
            caller,
            U256::from(0),
        );
        let mut new_bytecode: Option<*const u8> = None;
        let mut new_pc: Option<usize> = None;
        let mut new_stack: Option<Vec<U256>> = None;
        if post_exec.is_some() {
            unsafe {
                new_pc = Some(post_exec.as_ref().unwrap().1);
                new_bytecode = Some(call.bytecode.as_ptr().add(new_pc.unwrap()));
                new_stack = Some(post_exec.unwrap().0);
            }
        }
        let mut interp = Interpreter::new::<LatestSpec>(call, 1e10 as u64);
        if new_stack.is_some() {
            unsafe {
                for v in new_stack.unwrap() {
                    interp.stack.push(v);
                }
                interp.instruction_pointer = new_bytecode.unwrap();
            }
        }
        let r = interp.run::<FuzzHost, LatestSpec>(&mut self.host);
        IntermediateExecutionResult {
            output: interp.return_value(),
            new_state: self.host.data.clone(),
            pc: interp.program_counter(),
            ret: r,
            stack: interp.stack.data().clone(),
        }
    }
}
