import { Registry } from 'prom-client';

import { HelloWorldChecker } from '@hyperlane-xyz/helloworld';
import {
  CheckerViolation,
  HypERC20App,
  HypERC20Checker,
  HyperlaneCoreChecker,
  HyperlaneIgp,
  HyperlaneIgpChecker,
  HyperlaneIsmFactory,
  InterchainAccount,
  InterchainAccountChecker,
  InterchainAccountConfig,
  InterchainQuery,
  InterchainQueryChecker,
  attachContractsMapAndGetForeignDeployments,
  hypERC20factories,
  proxiedFactories,
} from '@hyperlane-xyz/sdk';
import { eqAddress, objFilter } from '@hyperlane-xyz/utils';

import { Contexts } from '../../config/contexts.js';
import { DEPLOYER } from '../../config/environments/mainnet3/owners.js';
import { getWarpAddresses } from '../../config/registry.js';
import { getWarpConfig } from '../../config/warp.js';
import { DeployEnvironment } from '../../src/config/environment.js';
import { HyperlaneAppGovernor } from '../../src/govern/HyperlaneAppGovernor.js';
import { HyperlaneCoreGovernor } from '../../src/govern/HyperlaneCoreGovernor.js';
import { HyperlaneHaasGovernor } from '../../src/govern/HyperlaneHaasGovernor.js';
import { HyperlaneIgpGovernor } from '../../src/govern/HyperlaneIgpGovernor.js';
import { ProxiedRouterGovernor } from '../../src/govern/ProxiedRouterGovernor.js';
import { Role } from '../../src/roles.js';
import { impersonateAccount, useLocalProvider } from '../../src/utils/fork.js';
import { logViolationDetails } from '../../src/utils/violation.js';
import {
  Modules,
  getArgs as getRootArgs,
  withAsDeployer,
  withChains,
  withContext,
  withFork,
  withGovern,
  withModule,
  withPushMetrics,
  withWarpRouteId,
} from '../agent-utils.js';
import { getEnvironmentConfig, getHyperlaneCore } from '../core-utils.js';
import { getHelloWorldApp } from '../helloworld/utils.js';

export function getCheckBaseArgs() {
  return withAsDeployer(
    withGovern(withChains(withFork(withContext(getRootArgs())))),
  );
}

export function getCheckWarpDeployArgs() {
  return withPushMetrics(getCheckBaseArgs());
}

export function getCheckDeployArgs() {
  return withWarpRouteId(withModule(getCheckBaseArgs()));
}

export async function getGovernor(
  module: Modules,
  context: Contexts,
  environment: DeployEnvironment,
  asDeployer: boolean,
  warpRouteId?: string,
  chains?: string[],
  fork?: string,
  govern?: boolean,
) {
  const envConfig = getEnvironmentConfig(environment);
  let multiProvider = await envConfig.getMultiProvider();

  // must rotate to forked provider before building core contracts
  if (fork) {
    await useLocalProvider(multiProvider, fork);

    if (govern) {
      multiProvider = multiProvider.extendChainMetadata({
        [fork]: { blocks: { confirmations: 0 } },
      });

      const owner = asDeployer ? DEPLOYER : envConfig.core[fork].owner;
      const signer = await impersonateAccount(owner, 1e18);

      multiProvider.setSigner(fork, signer);
    }
  }

  const { core, chainAddresses } = await getHyperlaneCore(
    environment,
    multiProvider,
  );

  let governor: HyperlaneAppGovernor<any, any>;

  const ismFactory = HyperlaneIsmFactory.fromAddressesMap(
    chainAddresses,
    multiProvider,
  );

  const routerConfig = core.getRouterConfig(envConfig.owners);

  const icaChainAddresses = objFilter(
    chainAddresses,
    (chain, _): _ is Record<string, string> =>
      !!chainAddresses[chain]?.interchainAccountRouter,
  );
  const ica = InterchainAccount.fromAddressesMap(
    icaChainAddresses,
    multiProvider,
  );

  if (module === Modules.CORE) {
    const checker = new HyperlaneCoreChecker(
      multiProvider,
      core,
      envConfig.core,
      ismFactory,
      chainAddresses,
    );
    governor = new HyperlaneCoreGovernor(checker, ica);
  } else if (module === Modules.INTERCHAIN_GAS_PAYMASTER) {
    const igp = HyperlaneIgp.fromAddressesMap(chainAddresses, multiProvider);
    const checker = new HyperlaneIgpChecker(multiProvider, igp, envConfig.igp);
    governor = new HyperlaneIgpGovernor(checker);
  } else if (module === Modules.INTERCHAIN_ACCOUNTS) {
    const checker = new InterchainAccountChecker(
      multiProvider,
      ica,
      objFilter(
        routerConfig,
        (chain, _): _ is InterchainAccountConfig => !!icaChainAddresses[chain],
      ),
    );
    governor = new ProxiedRouterGovernor(checker);
  } else if (module === Modules.HAAS) {
    const icaChecker = new InterchainAccountChecker(
      multiProvider,
      ica,
      objFilter(
        routerConfig,
        (chain, _): _ is InterchainAccountConfig => !!icaChainAddresses[chain],
      ),
    );
    const coreChecker = new HyperlaneCoreChecker(
      multiProvider,
      core,
      envConfig.core,
      ismFactory,
      chainAddresses,
    );
    governor = new HyperlaneHaasGovernor(ica, icaChecker, coreChecker);
  } else if (module === Modules.INTERCHAIN_QUERY_SYSTEM) {
    const iqs = InterchainQuery.fromAddressesMap(chainAddresses, multiProvider);
    const checker = new InterchainQueryChecker(
      multiProvider,
      iqs,
      routerConfig,
    );
    governor = new ProxiedRouterGovernor(checker);
  } else if (module === Modules.HELLO_WORLD) {
    const app = await getHelloWorldApp(
      envConfig,
      context,
      Role.Deployer,
      Contexts.Hyperlane, // Owner should always be from the hyperlane context
    );
    const ismFactory = HyperlaneIsmFactory.fromAddressesMap(
      chainAddresses,
      multiProvider,
    );
    const checker = new HelloWorldChecker(
      multiProvider,
      app,
      routerConfig,
      ismFactory,
    );
    governor = new ProxiedRouterGovernor(checker);
  } else if (module === Modules.WARP) {
    if (!warpRouteId) {
      throw new Error('Warp route id required for warp module');
    }
    const config = await getWarpConfig(multiProvider, envConfig, warpRouteId);
    const warpAddresses = getWarpAddresses(warpRouteId);
    const filteredAddresses = Object.keys(warpAddresses) // filter out changes not in config
      .filter((key) => key in config)
      .reduce((obj, key) => {
        obj[key] = {
          ...warpAddresses[key],
        };

        // if the owner in the config is an AW account, set the proxyAdmin to the AW singleton proxyAdmin
        // this will ensure that the checker will check that any proxies are owned by the singleton proxyAdmin
        const proxyAdmin = eqAddress(
          config[key].owner,
          envConfig.owners[key]?.owner,
        )
          ? chainAddresses[key]?.proxyAdmin
          : undefined;

        if (proxyAdmin) {
          obj[key].proxyAdmin = proxyAdmin;
        }

        return obj;
      }, {} as typeof warpAddresses);

    const { contractsMap, foreignDeployments } =
      attachContractsMapAndGetForeignDeployments(
        filteredAddresses,
        { ...hypERC20factories, ...proxiedFactories },
        multiProvider,
      );

    // log error and return if requesting check on foreign deployment
    const nonEvmChains = chains
      ? chains.filter((c) => foreignDeployments[c])
      : fork && foreignDeployments[fork]
      ? [fork]
      : [];

    if (nonEvmChains.length > 0) {
      const chainList = nonEvmChains.join(', ');
      console.log(
        `${chainList} ${
          nonEvmChains.length > 1 ? 'are' : 'is'
        } non-EVM and not compatible with warp checker tooling`,
      );
      throw Error(
        `${chainList} ${
          nonEvmChains.length > 1 ? 'are' : 'is'
        } non-EVM and not compatible with warp checker tooling`,
      );
    }

    const app = new HypERC20App(
      contractsMap,
      multiProvider,
      undefined,
      foreignDeployments,
    );

    const checker = new HypERC20Checker(
      multiProvider,
      app,
      config as any,
      ismFactory,
    );
    governor = new ProxiedRouterGovernor(checker, ica);
  } else {
    throw Error(
      `Checker or governor not implemented not implemented for ${module}`,
    );
  }

  return governor;
}

export function logViolations(violations: CheckerViolation[]) {
  console.table(violations, [
    'chain',
    'remote',
    'name',
    'type',
    'subType',
    'actual',
    'expected',
  ]);
  logViolationDetails(violations);
}

export function getCheckerViolationsGaugeObj(metricsRegister: Registry) {
  return {
    name: 'hyperlane_check_violations',
    help: 'Checker violation',
    registers: [metricsRegister],
    labelNames: [
      'module',
      'warp_route_id',
      'chain',
      'remote',
      'contract_name',
      'type',
      'sub_type',
      'actual',
      'expected',
    ],
  };
}